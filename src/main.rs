// Copyright (c) 2021, Qualcomm Innovation Center, Inc. All rights reserved.
//
// SPDX-License-Identifier: BSD-3-Clause
use clap::{arg_enum, App, AppSettings, Arg, ArgMatches};
use indicatif::{ProgressBar, ProgressStyle};
use is_executable::IsExecutable;
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate clap;
extern crate num_cpus;

pub mod afl;
pub mod gdb_triage;
pub mod platform;
pub mod process;
pub mod report;
pub mod util;

use gdb_triage::{GdbTriageError, GdbTriageResult, GdbTriager};
use process::ChildResult;

const VERSION: &str = env!("CARGO_PKG_VERSION");

arg_enum! {
    #[derive(PartialEq, Debug)]

    // these are user facing
    #[allow(non_camel_case_types)]
    pub enum OutputFormat {
        text,
        markdown,
        json
    }
}

fn setup_command_line() -> ArgMatches<'static> {
    let mut app = App::new("afltriage")
                          .version(crate_version!())
                          .author(crate_authors!("\n"))
                          .about(crate_description!())
                          .usage("afltriage -i <input>... -o <output> <command>...")
                          .setting(AppSettings::TrailingVarArg)
                          .setting(AppSettings::DontDelimitTrailingValues)
                          .setting(AppSettings::DontCollapseArgsInUsage)
                          .setting(AppSettings::UnifiedHelpMessage)
                          .arg(Arg::with_name("input")
                               .short("-i")
                               .takes_value(true)
                               .required(true)
                               .multiple(true)
                               .help("A list of paths to a testcase, directory of testcases, AFL directory, and/or directory of AFL directories to be triaged.")
                               .long_help("A list of paths to a testcase, directory of testcases, AFL directory, \
                                     and/or directory of AFL directories to be triaged. Note that this arg \
                                     takes multiple inputs in a row (e.g. -i input1 input2...) so it cannot be the last \
                                     argument passed to AFLTriage -- this is reserved for the command."))
                          .arg(Arg::with_name("profile_only")
                               .long("--profile-only")
                               .takes_value(false)
                               .help("Perform environment checks, describe the inputs to be triaged, and profile the target binary."))
                          .arg(Arg::with_name("skip_profile")
                               .long("--skip-profile")
                               .takes_value(false)
                               .conflicts_with("profile_only")
                               .help("Skip target profiling before input processing."))
                          .arg(Arg::with_name("output")
                               .short("-o")
                               .takes_value(true)
                               .required(true)
                               .help("The output path for triage report files. Use '-' to print to console."))
                          .arg(Arg::with_name("jobs")
                               .short("-j")
                               .takes_value(true)
                               .help("How many threads to use during triage."))
                          .arg(Arg::with_name("timeout")
                               .short("-t")
                               .long("--timeout")
                               .default_value("60000")
                               .takes_value(true)
                               .help("The timeout in milliseconds for each testcase to triage."))
                          .arg(Arg::with_name("debug")
                               .long("--debug")
                               .help("Enable low-level debugging of triage operations."))
                          .arg(Arg::with_name("child_output")
                               .long("--child-output")
                               .help("Include child output in triage reports."))
                          .arg(Arg::with_name("child_output_lines")
                               .long("--child-output-lines")
                               .default_value("25")
                               .takes_value(true)
                               .help("How many lines of program output from the target to include in reports. Use 0 to mean unlimited lines (not recommended)."))
                          .arg(Arg::with_name("ofmt")
                               .long("--output-format")
                               .takes_value(true)
                               .possible_values(&OutputFormat::variants())
                               .default_value("text")
                               .required(false)
                               .case_insensitive(true)
                               .help("The triage report output format."))
                          .arg(Arg::with_name("stdin")
                               .long("--stdin")
                               .takes_value(false)
                               .help("Provide testcase input to the target via stdin instead of a file."))
                          .arg(Arg::with_name("command")
                               .multiple(true)
                               .required(true)
                               .help("The binary executable and args to execute. Use '@@' as a placeholder for the path to the input file or --stdin. Optionally use -- to delimit the start of the command."));

    if env::args().len() <= 1 {
        app.print_help().unwrap();
        println!();
        std::process::exit(0);
    }

    app.get_matches()
}

struct TriageState {
    crashed: usize,
    no_crash: usize,
    timedout: usize,
    errored: usize,
    crash_signature: HashSet<String>,
    unique_errors: HashMap<GdbTriageError, usize>,
}

enum TriageResult {
    NoCrash(ChildResult),
    Crash(GdbTriageResult),
    Error(GdbTriageError),
    Timedout,
}

struct ProfileResult {
    process_result: std::io::Result<ChildResult>,
    process_execution_time: Duration,
    process_rss: usize,
    triage_result: TriageResult,
    debugger_execution_time: Duration,
    debugger_rss: usize,
    debug_mem_overhead: f32,
    debug_time_overhead: f32,
}

fn profile_target(
    gdb: &GdbTriager,
    binary_args: &[&str],
    testcase: &str,
    debug: bool,
    input_stdin: bool,
    timeout_ms: u64,
) -> std::io::Result<ProfileResult> {
    log::info!("Profiling target...");

    let prog_args = expand_filepath_templates(binary_args, testcase);

    let input_file = if input_stdin {
        Some(util::read_file_to_bytes(testcase)?)
    } else {
        None
    };

    let start = Instant::now();
    let before_rss = util::get_peak_rss();
    let process_result = process::execute_capture_output_timeout(&prog_args[0], &prog_args[1..], timeout_ms, input_file);
    let process_execution_time = start.elapsed();
    let after_process_rss = util::get_peak_rss();
    let process_rss = std::cmp::max(after_process_rss - before_rss, 1); // round up to 1kb

    log::info!("Target profile: time={:?}, mem={}KB",
        process_execution_time, process_rss);

    let start = Instant::now();
    let triage_result = triage_test_case(gdb, binary_args, testcase, debug, input_stdin, timeout_ms);
    let debugger_execution_time = start.elapsed();
    let after_debugger_rss = util::get_peak_rss();

    let debugger_rss = std::cmp::max(after_debugger_rss - after_process_rss, 1);

    let debug_time_overhead = (debugger_execution_time.as_millis() as f32) /
        (process_execution_time.as_millis() as f32);

    let debug_mem_overhead = (debugger_rss as f32) /
        (process_rss as f32);

    log::info!("Debugged profile: t={:?} ({:.2}x), mem={}KB ({:.2}x)",
        debugger_execution_time, debug_time_overhead, debugger_rss, debug_mem_overhead);

    Ok(ProfileResult {
        process_result,
        process_execution_time,
        process_rss,
        triage_result,
        debugger_execution_time,
        debugger_rss,
        debug_mem_overhead,
        debug_time_overhead,
    })
}

fn expand_filepath_templates(args: &[&str], value: &str) -> Vec<String> {
    let mut expanded_args: Vec<String> = Vec::new();

    for arg in args.iter() {
        if *arg == "@@" {
            expanded_args.push(value.to_string());
        } else {
            expanded_args.push((*arg).to_string());
        }
    }

    expanded_args
}

fn triage_test_case(
    gdb: &GdbTriager,
    binary_args: &[&str],
    testcase: &str,
    debug: bool,
    input_stdin: bool,
    timeout_ms: u64,
) -> TriageResult {
    let prog_args = expand_filepath_templates(binary_args, testcase);

    // Whether to pass a file in via GDB stdin
    let input_file = if input_stdin { Some(testcase) } else { None };

    let triage_result: GdbTriageResult =
        match gdb.triage_program(&prog_args, input_file, debug, timeout_ms) {
            Ok(triage_result) => triage_result,
            Err(e) => {
                if e.error_kind == gdb_triage::GdbTriageErrorKind::Timeout {
                    return TriageResult::Timedout;
                } else {
                    return TriageResult::Error(e);
                }
            }
        };

    if triage_result.response.result.is_none() {
        TriageResult::NoCrash(triage_result.child)
    } else {
        TriageResult::Crash(triage_result)
    }
}

enum UserInputPathType {
    Unknown,
    Missing,
    Single,
    PlainDir,
    AflDir,
}

struct UserInputPath {
    ty: UserInputPathType,
    path: PathBuf,
    fuzzer_stats: Option<afl::AflStats>,
}

struct Testcase {
    path: PathBuf,
    /// Must be safe for filesystem
    #[allow(dead_code)]
    unique_id: String,
}

fn determine_input_type(input: &Path) -> UserInputPathType {
    let metadata = match input.symlink_metadata() {
        Ok(meta) => meta,
        Err(_) => return UserInputPathType::Missing,
    };

    if metadata.file_type().is_file() {
        return UserInputPathType::Single;
    }

    // looks like an AFL dir
    if input.join("fuzzer_stats").exists()
        || input.join("queue").exists()
        || input.join("crashes").exists()
    {
        return UserInputPathType::AflDir;
    }

    if metadata.file_type().is_dir() {
        return UserInputPathType::PlainDir;
    }

    UserInputPathType::Unknown
}

fn sanity_check(gdb: &GdbTriager, binary_args: &[&str]) -> bool {
    let rawexe = binary_args.get(0).unwrap();
    let exe = PathBuf::from(rawexe);
    let justfilename = exe
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(""))
        .to_str()
        .unwrap();

    // A PATH resolvable name
    if justfilename == *rawexe {
        if which::which(rawexe).is_err() {
            log::error!(
                "Binary {} not found in PATH. Try using the absolute path",
                rawexe
            );
            return false;
        }
    } else if !exe.is_executable() {
        log::error!("Binary {} does not exist or is not executable", rawexe);
        return false;
    }

    if !gdb.has_supported_gdb() {
        return false;
    }

    // Undocumented Glibc env var that prevents it from printing to /dev/tty, which isn't captured by GDB
    // https://stackoverflow.com/questions/32056387/catching-libc-error-messages-redirecting-from-dev-tty
    env::set_var("LIBC_FATAL_STDERR_", "1");

    match env::var("ASAN_OPTIONS") {
        Ok(val) => {
            log::warn!("Using ASAN_OPTIONS=\"{}\" that was set by the environment. This can change triage result accuracy", val);

            let re = Regex::new(r"abort_on_error=(1|true)").unwrap();
            if re.find(&val).is_none() {
                log::error!("ASAN_OPTIONS does not have required abort_on_error=1 option");
                return false;
            }
        }
        Err(_) => env::set_var(
            "ASAN_OPTIONS",
            "abort_on_error=1:allow_user_segv_handler=0:symbolize=1,detect_leaks=0",
        ),
    }

    match env::var("ASAN_SYMBOLIZER_PATH") {
        Ok(val) => {
            log::info!(
                "Using ASAN_SYMBOLIZER_PATH=\"{}\" that was set by the environment",
                val
            );
        }
        Err(_) => match which::which("addr2line") {
            Ok(path) => {
                env::set_var("ASAN_SYMBOLIZER_PATH", path.to_str().unwrap());
                log::info!("Using ASAN_SYMBOLIZER_PATH=\"{}\"", path.to_str().unwrap());
            }
            _ => {
                log::warn!("No ASAN_SYMBOLIZER_PATH found. Consider setting it to llvm-symbolizer or addr2line if your target is using ASAN");
            }
        },
    }

    true
}

fn collect_input_testcases(processed_inputs: &mut Vec<UserInputPath>) -> Vec<Testcase> {
    let mut all_testcases = Vec::new();

    for input in processed_inputs {
        let path_str = input.path.to_str().unwrap();

        match input.ty {
            UserInputPathType::Single => {
                log::info!("Triaging single {}", path_str);
                all_testcases.push(Testcase {
                    unique_id: "".to_string(),
                    path: input.path.clone(),
                });
            }
            UserInputPathType::PlainDir => {
                if let Ok(tcs) = afl::afl_list_testcases(input.path.as_path()) {
                    let mut valid = 0;
                    for tc in tcs {
                        if tc.is_file() {
                            valid += 1;
                            all_testcases.push(Testcase {
                                unique_id: "".to_string(),
                                path: tc,
                            });
                        }
                    }

                    if valid > 0 {
                        log::info!("Triaging plain directory {} ({} files)", path_str, valid);
                    } else {
                        log::warn!("No files found in directory {}", path_str);
                    }
                } else {
                    log::warn!("Failed to get files from directory {}", path_str);
                }
            }
            UserInputPathType::AflDir => {
                match afl::afl_list_testcases(input.path.join("crashes").as_path()) {
                    Ok(tcs) => {
                        let mut valid = 0;
                        for tc in tcs {
                            if tc.is_file() {
                                // TODO: filter command (.*id:.*)
                                if tc.file_name().unwrap() == "README.txt" {
                                    continue;
                                }

                                valid += 1;
                                all_testcases.push(Testcase {
                                    unique_id: "".to_string(),
                                    path: tc,
                                });
                            }
                        }

                        if valid > 0 {
                            log::info!("Triaging AFL directory {} ({} files)", path_str, valid);
                        } else {
                            log::warn!("No crashes found in AFL directory {}", path_str);
                        }
                    }
                    Err(e) => log::warn!(
                        "Failed to get AFL crashes from directory {}: {}",
                        path_str,
                        e
                    ),
                }

                let fuzzer_stats =
                    match afl::parse_afl_fuzzer_stats(input.path.join("fuzzer_stats").as_path()) {
                        Ok(s) => match afl::validate_afl_fuzzer_stats(&s) {
                            Ok(s2) => {
                                log::info!("├─ Banner: {}", s2.afl_banner);
                                log::info!("├─ Command line: \"{}\"", s2.command_line);
                                log::info!("└─ Paths found: {}", s2.paths_total);
                                Some(s2)
                            }
                            Err(e) => {
                                log::warn!("Failed to validate AFL fuzzer_stats: {}", e);
                                None
                            }
                        },
                        Err(_e) => {
                            log::warn!("AFL directory is missing fuzzer_stats");
                            None
                        }
                    };

                input.fuzzer_stats = fuzzer_stats;
            }
            _ => log::warn!("Skipping unknown or missing path {}", path_str),
        }
    }

    all_testcases
}

fn init_logger() {
    use env_logger::{fmt::Color, Builder, Env, Target};
    use log::{Level, LevelFilter};
    use std::io::Write;

    let env = Env::default();

    Builder::from_env(env)
        .filter_level(LevelFilter::Info)
        .target(Target::Stdout)
        .format(|buf, record| {
            let mut style = buf.style();

            let _timestamp = buf.timestamp();

            let level_name = match record.level() {
                Level::Info => "[+]".to_string(),
                Level::Warn => {
                    style.set_color(Color::Yellow).set_intense(true);
                    "[!]".to_string()
                }
                Level::Error => {
                    style.set_color(Color::Red).set_intense(true);
                    "[X]".to_string()
                }
                _ => format!("[{}]", record.level().to_string()),
            };

            writeln!(
                buf,
                "{} {}",
                style.value(level_name),
                style.value(record.args())
            )
        })
        .init();
}

fn main() {
    std::process::exit(main_wrapper());
}

fn main_wrapper() -> i32 {
    /* AFLTriage Flow
     *
     * 1. Environment sanity check: gdb python, binary exists
     * 2. Input processing: for each input path determine single file, dir with files, afl dir single, afl
     *    dir primary/secondaries
     *      - Reject AFL dirs for multiple different fuzzers and provide guidance for this
     * 3. Input collection: resolve all paths to input files in a stable order
     *      - Convert paths to unique identifiers for report writing
     * 4. Triaging: collect crash info, process crash info, classify/dedup
     *      - Write report in text/json
     */

    let args = setup_command_line();

    println!("AFLTriage v{} by Grant Hernandez\n", VERSION);
    init_logger();

    let binary_args: Vec<&str> = args.values_of("command").unwrap().collect();

    // TODO: fix binary_args validation
    let gdb: GdbTriager = GdbTriager::new();

    if !sanity_check(&gdb, &binary_args) {
        return 1;
    }

    let input_stdin = args.is_present("stdin");
    let has_atat = binary_args.iter().any(|s| *s == "@@");

    if input_stdin {
        log::info!("Providing testcase input via stdin");

        if has_atat {
            log::warn!("Image triage args contains @@ but you are using --stdin");
        }
    } else {

        if !has_atat {
            log::error!("Image triage args missing file placeholder: @@. If you'd like to pass input to the child via stdin, use the --stdin option.");
            return 1;
        }
    }

    let binary_cmdline = binary_args.join(" ");

    log::info!("Image triage cmdline: \"{}\"", binary_cmdline);

    let output = args.value_of("output").unwrap();

    // Output to the terminal
    let output_dir = if output == "-" {
        None
    } else {
        let d = std::path::PathBuf::from(output);

        if let Err(e) = std::fs::create_dir(&d) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                log::error!("Error creating output directory: {}", e);
                return 1;
            }
        }

        Some(d)
    };

    match &output_dir {
        Some(_) => log::info!("Reports will be output to directory \"{}\"", output),
        None => log::info!("Reports output to terminal"),
    }

    let input_paths: Vec<&str> = args.values_of("input").unwrap().collect();

    let mut processed_inputs = Vec::new();

    for input in input_paths {
        let path = PathBuf::from(input);
        let ty = determine_input_type(&path);

        processed_inputs.push(UserInputPath {
            ty,
            path,
            fuzzer_stats: None,
        });
    }

    let all_testcases = collect_input_testcases(&mut processed_inputs);

    if all_testcases.is_empty() {
        log::error!("No testcases found!");
        return 1;
    }

    let debug = args.is_present("debug");
    let child_output = args.is_present("child_output");

    let child_output_lines = if let Ok(n) = value_t!(args, "child_output_lines", usize) {
        n
    } else {
        log::error!("Child output lines parse error");
        return 1;
    };

    let timeout_ms = value_t!(args, "timeout", u64).unwrap_or_else(|_| 60000);

    if timeout_ms < 100 {
        log::warn!("Requested timeout of {}ms is dangerously low!", timeout_ms);
    } else {
        log::info!("Triage timeout set to {}ms", timeout_ms);
    }

    let mut max_recommended_threadcount = num_cpus::get();

    if !args.is_present("skip_profile") {
        let first_testcase_path = all_testcases[0].path.to_str().unwrap();
        let profile_result = profile_target(&gdb, &binary_args, first_testcase_path, debug, input_stdin, timeout_ms);

        if let Ok(profile_result) = profile_result {
            if let std::io::Result::Err(e) = profile_result.process_result {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    log::warn!("Target process timed out during profiling! Consider raising the timeout")
                } else {
                    log::error!("Target process errored during profiling: {}", e);
                    log::error!("It's unlikely that triage will succeed - exiting...");
                    return 1;
                }
            }

            match profile_result.triage_result {
                TriageResult::Timedout => {
                    log::warn!("The triage process timed out during profiling! The debugger needs to load and process symbols, which increases execution time. Consider raising the timeout")
                },
                TriageResult::Error(err) => {
                    log::error!("The triage errored during profiling (enable --debug for more information): {}", err.to_string());
                    log::error!("It's unlikely that triage will succeed - exiting...");
                    return 1;
                }
                _ => (),
            }

            if let Some(memkb) = util::read_available_memory() {
                log::info!("System memory available: {} KB", memkb);
                log::info!("System cores available: {}", max_recommended_threadcount);
                let new_thread_count = (memkb as usize) / profile_result.debugger_rss;

                if new_thread_count < max_recommended_threadcount {
                    log::warn!("The target is memory hungry - reducing threadcount from {} to {}",
                        max_recommended_threadcount, new_thread_count);
                    max_recommended_threadcount = new_thread_count;
                }

            } else {
                log::warn!("Unable to determine available system memory");
            }
        } else {
            log::error!("Failed to read profile input testcase");
            return 1;
        }
    }

    if args.is_present("profile_only") {
        log::info!("Exiting due to --profile-only");
        return 0;
    }

    let requested_job_count = if let Ok(v) = value_t!(args, "jobs", usize) {
        if v > max_recommended_threadcount {
            log::warn!("Requested thread count of {} may exceed system resources", v);
        }
        v
    } else {
        max_recommended_threadcount
    };

    // No point in having more threads than testcases...
    let job_count = std::cmp::max(1, std::cmp::min(requested_job_count, all_testcases.len()));

    //////////////////

    log::info!("Triaging {} testcases", all_testcases.len());
    log::info!("Using {} threads to triage", job_count);

    rayon::ThreadPoolBuilder::new()
        .num_threads(job_count)
        .build_global()
        .unwrap();

    let pb = ProgressBar::new((&all_testcases).len() as u64);

    let display_progress = util::isatty() && output_dir.is_some() && !debug;

    if display_progress {
        pb.set_style(ProgressStyle::default_bar()
                     .template("[+] Triaging {spinner:.green} [{pos}/{len} {elapsed_precise}] [{bar:.cyan/blue}] {wide_msg}")
                     .progress_chars("#>-"));
        pb.enable_steady_tick(200);
    }

    let write_message: Box<dyn Fn(String, Option<&str>) + Sync> = if display_progress {
        Box::new(|msg, tc| {
            pb.set_message(&msg)
        })
    } else {
        Box::new(|msg, tc| {
            if let Some(tc_name) = tc {
                log::info!("{}: {}", tc_name, msg)
            } else {
                log::info!("{}", msg)
            }
        })
    };

    write_message(format!("Processing initial {} test cases", job_count), None);

    let state = Arc::new(Mutex::new(TriageState {
        crashed: 0,
        no_crash: 0,
        errored: 0,
        timedout: 0,
        crash_signature: HashSet::new(),
        unique_errors: HashMap::new(),
    }));

    all_testcases.par_iter().panic_fuse().for_each(|testcase| {
        let path = testcase.path.to_str().unwrap();
        let result = triage_test_case(&gdb, &binary_args, path, debug, input_stdin, timeout_ms);

        let report = match &result {
            TriageResult::Crash(triage) => Some(report::format_text_report(triage)),
            _ => None,
        };

        // do very little with this lock held. do not reorder
        let mut state = state.lock().unwrap();

        // TODO: display child-output even without a crash to help debug triage errors

        match result {
            TriageResult::NoCrash(_child) => {
                state.no_crash += 1;

                if !display_progress {
                    write_message("No crash".into(), Some(path));
                }
            }
            TriageResult::Timedout => {
                state.timedout += 1;

                if !display_progress {
                    write_message("Timed out".into(), Some(path));
                }
            }
            TriageResult::Crash(triage) => {
                let report = report.as_ref().unwrap();

                state.crashed += 1;

                if !state.crash_signature.contains(&report.stackhash) {
                    write_message(format!("{}", report.headline), Some(path));

                    state.crash_signature.insert(report.stackhash.to_string());

                    let mut text_report = format!(
                        "Summary: {}\nCommand line: {}\nTestcase: {}\nStack hash: {}\n\n",
                        report.headline, binary_cmdline, path, report.stackhash
                    );

                    text_report += &format!("Register info:\n{}\n", report.register_info);
                    text_report += &format!("Crash context:\n{}\n", report.crash_context);
                    text_report += &format!("Crashing thread backtrace:\n{}\n", report.backtrace);

                    if !report.asan_body.is_empty() {
                        text_report += &format!("ASAN Report:\n{}\n", report.asan_body);
                    }

                    let mut format_output = |name: &str, output: &str| {
                        if output.is_empty() {
                            text_report.push_str(&format!("\nChild {} (no output):\n", name));
                        } else if child_output_lines == 0 {
                            text_report
                                .push_str(&format!("\nChild {} (everything):\n{}\n", name, output));
                        } else {
                            let lines = util::tail_string(output, child_output_lines);
                            text_report.push_str(&format!(
                                "\nChild {} (last {} lines):\n",
                                name, child_output_lines
                            ));
                            for (i, line) in lines.iter().enumerate() {
                                if line.is_empty() && i + 1 == lines.len() {
                                    break;
                                }
                                text_report.push_str(&format!("{}\n", line));
                            }
                        }
                    };

                    if child_output {
                        // Dont include the ASAN report duplicated in the child's STDERR
                        let stderr = if report.asan_body.is_empty() {
                            triage.child.stderr
                        } else {
                            triage
                                .child
                                .stderr
                                .replace(&report.asan_body, "<ASAN Report>")
                        };

                        format_output("STDOUT", &triage.child.stdout);
                        format_output("STDERR", &stderr);
                    }

                    if output_dir.is_none() {
                        write_message(format!(
                            "--- REPORT BEGIN ---\n{}\n--- REPORT END ---",
                            text_report,
                        ), None);
                    } else {
                        let output_dir = output_dir.as_ref().unwrap();
                        let report_filename = format!(
                            "afltriage_{}_{}.txt",
                            util::sanitize(&report.terse_headline),
                            &report.stackhash[..8]
                        );

                        if let Err(e) =
                            std::fs::write(output_dir.join(report_filename), text_report)
                        {
                            // TODO: notify / exit early
                            let failed_to_write = format!("Failed to write report: {}", e);
                            write_message(failed_to_write, Some(path));
                        }
                    }
                } else {
                    write_message(format!("{}", report.headline), Some(path));
                }
            }
            TriageResult::Error(gdb_error) => {
                state.errored += 1;

                write_message(format!("ERROR: {}", gdb_error.error), Some(path));

                if let Some(x) = state.unique_errors.get_mut(&gdb_error) {
                    *x += 1;
                } else {
                    state.unique_errors.insert(gdb_error, 1);
                }
            }
        };

        if display_progress {
            pb.inc(1);
        }
    });

    if display_progress {
        pb.finish();
    } else {
        pb.finish_and_clear();
    }

    let state = state.lock().unwrap();
    let total = all_testcases.len();

    log::info!(
        "Triage stats [Crashes: {} (unique {}), No crash: {}, Timeout: {}, Errored: {}]",
        state.crashed,
        state.crash_signature.len(),
        state.no_crash,
        state.timedout,
        state.errored
    );

    if state.errored == total {
        log::error!("Something seems to be wrong during triage as all testcases errored.");
    }

    if state.errored > 0 {
        log::warn!(
            "There were {} error(s) ({} unique) during triage",
            state.errored,
            state.unique_errors.len()
        );

        for (err, times) in &state.unique_errors {
            let times = format!(" (seen {} time(s))", times);
            log::error!("Triage error {}: {}", times, err.to_string());
        }
    }

    if state.no_crash == total {
        log::warn!("None of the testcases crashed! Make sure that you are using the correct target command line and the right set of testcases");
    }

    if state.timedout == total {
        log::warn!("All of the testcases timed out! Try increasing the timeout (debugger symbol loading can increase triage time) and double check you are using the right command line.");
    }

    return 0;
}
