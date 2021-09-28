// Copyright (c) 2021, Qualcomm Innovation Center, Inc. All rights reserved.
//
// SPDX-License-Identifier: BSD-3-Clause
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::rc::Rc;

use crate::process::{self, ChildResult};

const INTERNAL_TRIAGE_SCRIPT: &[u8] = include_bytes!("./res/GDBTriage.py");

#[derive(Debug, Serialize, PartialEq, Deserialize)]
pub struct GdbSymbol {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangled_function_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<Rc<GdbVariable>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locals: Option<Vec<Rc<GdbVariable>>>,
}

impl GdbSymbol {
    pub fn format(&self) -> String {
        self.format_short()
    }

    pub fn format_short(&self) -> String {
        self.function_name
            .as_ref()
            .unwrap_or(&String::from(""))
            .to_string()
    }

    pub fn format_function_prototype(&self) -> String {
        let return_type = match &self.function_signature {
            Some(rv) => match rv.find(' ') {
                Some(pos) => rv[..=pos].to_string(),
                None => "".to_string(),
            },
            None => "".to_string(),
        };

        let args = self.args.as_ref().map_or_else(
            || String::from(""),
            |args| {
                args.iter()
                    .map(|x| x.format_arg())
                    .collect::<Vec<String>>()
                    .join(", ")
            },
        );

        return format!("{}{}({})", return_type, self.format_short(), args);
    }

    pub fn format_function_call(&self) -> String {
        let args = self.args.as_ref().map_or_else(
            || String::from("???"),
            |args| {
                args.iter()
                    .map(|x| x.name.as_str())
                    .collect::<Vec<&str>>()
                    .join(", ")
            },
        );

        return format!("{}({})", self.format_short(), args);
    }

    pub fn format_file(&self) -> String {
        let mut filename = String::new();

        if let Some(file) = &self.file {
            filename += file;
        }

        if let Some(line) = &self.line {
            filename += &format!(":{}", line);
        }

        filename
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GdbVariable {
    pub r#type: String,
    pub name: String,
    pub value: String,
}

impl GdbVariable {
    pub fn format_arg(&self) -> String {
        format!("{} = ({}){}", self.name, self.r#type, self.value)
    }

    pub fn format_decl(&self) -> String {
        format!("{} {} = {};", self.r#type, self.name, self.value)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GdbFrameInfo {
    pub address: u64,
    pub relative_address: u64,
    pub module: String,
    pub module_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Rc<GdbSymbol>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GdbThread {
    pub tid: i32,
    pub backtrace: Vec<GdbFrameInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_instruction: Option<String>,
    /// Registers are passed in the GDB defined order
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registers: Option<Vec<Rc<GdbRegister>>>,
}

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GdbRegister {
    pub name: String,
    // TODO: what about SIMD registers larger than 64 bits?
    pub value: u64,
    pub pretty_value: String,
    pub r#type: String,
    /// Size in bytes
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GdbStopInfo {
    pub signal_name: String,
    pub signal_number: i32,            // si_signo
    pub signal_code: i32,              // si_code
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faulting_address: Option<u64>, // sigfault.si_addr
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GdbArchInfo {
    pub address_bits: usize,
    pub architecture: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GdbContextInfo {
    pub stop_info: GdbStopInfo,
    pub arch_info: GdbArchInfo,
    pub primary_thread: GdbThread,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_threads: Option<Vec<GdbThread>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum GdbResultCode {
    SUCCESS,
    ERROR_TARGET_NOT_RUNNING,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GdbJsonResult {
    pub result: GdbResultCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<GdbContextInfo>,
}

#[derive(Debug)]
pub struct GdbTriageResult {
    pub response: GdbJsonResult,
    pub child: ChildResult, // TODO: don't pass exit code as its for gdb
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum GdbTriageErrorKind {
    Command,
    Internal,
    Timeout,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct GdbTriageError {
    pub error_kind: GdbTriageErrorKind,
    pub error: String,
    pub details: Vec<String>,
}

impl GdbTriageError {
    pub fn new(
        error_kind: GdbTriageErrorKind,
        error: &str,
        extra_detail: String,
    ) -> GdbTriageError {
        GdbTriageError {
            error_kind,
            error: error.to_string(),
            details: vec![extra_detail],
        }
    }

    pub fn new_brief(error_kind: GdbTriageErrorKind, error: &str) -> GdbTriageError {
        GdbTriageError {
            error_kind,
            error: error.to_string(),
            details: Vec::new(),
        }
    }

    pub fn new_detailed(
        error_kind: GdbTriageErrorKind,
        error: &str,
        details: Vec<String>,
    ) -> GdbTriageError {
        GdbTriageError {
            error_kind,
            error: error.to_string(),
            details,
        }
    }

    pub fn to_string(&self) -> String {
        if self.details.is_empty() {
            format!("{}", self.error)
        } else if self.details.len() == 1 {
            format!(
                "{}: {}",
                self.error,
                self.details.get(0).unwrap().trim_end(),
            )
        } else {
            let mut msg = format!("{}\n", self.error);

            for (i, line) in self.details.iter().enumerate() {
                msg += &format!("{}: {}\n", i + 1, line.trim_end());
            }

            msg
        }
    }
}

macro_rules! vec_of_strings {
    ($($x:expr),*) => (vec![$($x.to_string()),*]);
}

struct DbgMarker {
    start: &'static str,
    end: &'static str,
    gdb_start: &'static str,
    gdb_end: &'static str,
}

impl DbgMarker {
    fn extract<'a>(&self, text: &'a str) -> Result<&'a str, String> {
        match text.find(&self.start) {
            Some(mut start_idx) => {
                match text.find(&self.end) {
                    Some(end_idx) => {
                        // assuming its printed as a newline
                        start_idx += self.start.len() + 1;

                        if start_idx <= end_idx {
                            Ok(&text[start_idx..end_idx])
                        } else {
                            Err(String::from("Start marker and end marker out-of-order"))
                        }
                    }
                    None => Err(format!("Could not find {}", self.end)),
                }
            }
            None => Err(format!("Could not find {}", self.start)),
        }
    }
}

// This is a neat trick to cut up the output we get from GDB into parts without having to
// join stderr and stdout into a single stream
// Some versions of GDB don't flush output before starting a child, so explicitly flush
macro_rules! make_gdb_marker {
    ( $string:expr ) => {
        concat!(
            "python [(x.write('",
            $string,
            "\\n'),x.flush()) for x in [sys.stdout, sys.stderr]]"
        )
    };
}

macro_rules! make_marker {
    ( $string:expr ) => {
        DbgMarker {
            start: concat!("----", $string, "_START----"),
            end: concat!("----", $string, "_END----"),
            gdb_start: make_gdb_marker!(concat!("----", $string, "_START----")),
            gdb_end: make_gdb_marker!(concat!("----", $string, "_END----")),
        }
    };
}

lazy_static! {
    static ref MARKER_CHILD_OUTPUT: DbgMarker = make_marker!("AFLTRIAGE_CHILD_OUTPUT");
    static ref MARKER_BACKTRACE: DbgMarker = make_marker!("AFLTRIAGE_BACKTRACE");
}

enum GdbTriageScript {
    #[allow(dead_code)]
    External(PathBuf),
    Internal(tempfile::NamedTempFile),
}

pub struct GdbTriager {
    triage_script: GdbTriageScript,
    gdb: String,
}

impl Default for GdbTriager {
    fn default() -> Self {
        Self::new()
    }
}

impl GdbTriager {
    pub fn new() -> GdbTriager {
        let mut triage_script =
            GdbTriageScript::Internal(tempfile::Builder::new().suffix(".py").tempfile().unwrap());

        if let GdbTriageScript::Internal(ref mut tf) = triage_script {
            tf.write_all(INTERNAL_TRIAGE_SCRIPT).unwrap();
        } else {
            panic!("Unsupported script path");
        }

        // TODO: allow user to select GDB
        GdbTriager {
            triage_script,
            gdb: "gdb".to_string(),
        }
    }

    pub fn has_supported_gdb(&self) -> bool {
        let python_cmd = "python import gdb, sys; print('V:'+gdb.execute('show version', to_string=True).splitlines()[0]); print('P:'+sys.version.splitlines()[0].strip())";
        let gdb_args = vec!["--nx", "--batch", "-iex", python_cmd];

        let output = match process::execute_capture_output(&self.gdb, &gdb_args) {
            Ok(o) => o,
            Err(e) => {
                log::error!("Failed to execute '{}': {}", &self.gdb, e);
                return false;
            }
        };

        let decoded_stdout = &output.stdout;
        let decoded_stderr = &output.stderr;

        let version = decoded_stdout
            .find("V:")
            .map(|start_idx| (&decoded_stdout[start_idx + 2..]).lines().next().unwrap());

        let python_version = decoded_stdout
            .find("P:")
            .map(|start_idx| (&decoded_stdout[start_idx + 2..]).lines().next().unwrap());

        if !output.status.success() || version == None || python_version == None {
            log::error!(
                "GDB sanity check failure\nARGS:{}\nSTDOUT: {}\nSTDERR: {}",
                gdb_args.join(" "),
                decoded_stdout,
                decoded_stderr
            );
            return false;
        }

        log::info!(
            "GDB is working ({} - Python {})",
            version.unwrap(),
            python_version.unwrap()
        );

        true
    }

    pub fn triage_program(
        &self,
        prog_args: &[String],
        input_file: Option<&str>,
        show_raw_output: bool,
        timeout_ms: u64,
    ) -> Result<GdbTriageResult, GdbTriageError> {
        let triage_script_path = if let GdbTriageScript::Internal(tf) = &self.triage_script {
            tf.path()
        } else {
            panic!("Unsupported triage script path")
        };

        let gdb_run_command = match input_file {
            // GDB overwrites args in the format (damn you)
            Some(file) => format!("run {} < \"{}\"", &prog_args[1..].join(" "), file),
            None => String::from("run"),
        };

        // TODO: memory limit?
        #[rustfmt::rustfmt_skip]
        let gdb_args = vec_of_strings!(
            "--nx", "--batch",
            // FIXME: index cache is a bit unreliable on earlier GDB versions
            //"-iex", "set index-cache on",
            //"-iex", "set index-cache directory gdb_cache",

            // Make special effort to get target output WITHOUT any GDB logging
            "-iex", "set print inferior-events off",
            // write the marker to both stdout and stderr as they are not interleaved
            // Markers will not print if logging is to /dev/null
            "-ex", MARKER_CHILD_OUTPUT.gdb_start,
            "-ex", "set logging file /dev/null",
            "-ex", "set logging redirect on",
            "-ex", "set logging on",
            "-ex", gdb_run_command,
            "-ex", "set logging redirect off",
            "-ex", "set logging off",
            "-ex", MARKER_CHILD_OUTPUT.gdb_end,
            "-ex", MARKER_BACKTRACE.gdb_start,
            "-x", triage_script_path.to_str().unwrap(),
            "-ex", "gdbtriage",
            "-ex", MARKER_BACKTRACE.gdb_end,
            "--args"
        );

        let gdb_cmdline = &[&gdb_args[..], prog_args].concat();
        let gdb_cmd_fmt = [std::slice::from_ref(&self.gdb), gdb_cmdline]
            .concat()
            .join(" ");

        // Never write to stdin for GDB as it can pass testcases to the target using "run < FILE"
        let output =
            match process::execute_capture_output_timeout(&self.gdb, gdb_cmdline, timeout_ms, None) {
                Ok(o) => o,
                Err(e) => {
                    return if e.kind() == ErrorKind::TimedOut {
                        Err(GdbTriageError::new(
                            GdbTriageErrorKind::Timeout,
                            "Timed out when triaging",
                            e.to_string(),
                        ))
                    } else {
                        Err(GdbTriageError::new(
                            GdbTriageErrorKind::Command,
                            "Failed to execute GDB command",
                            e.to_string(),
                        ))
                    };
                }
            };

        let decoded_stdout = &output.stdout;
        let decoded_stderr = &output.stderr;

        if show_raw_output {
            println!("--- RAW GDB BEGIN ---\nPROGRAM CMDLINE: {}\nGDB CMDLINE: {}\nSTDOUT:\n{}\nSTDERR:\n{}\n--- RAW GDB END ---",
                prog_args[..].join(" "), gdb_cmd_fmt, decoded_stdout, decoded_stderr);
        }

        let child_output_stdout = match MARKER_CHILD_OUTPUT.extract(decoded_stdout) {
            Ok(output) => output.to_string(),
            Err(e) => {
                return Err(GdbTriageError::new(
                    GdbTriageErrorKind::Command,
                    "Could not extract child STDOUT",
                    e,
                ))
            }
        };

        let child_output_stderr = match MARKER_CHILD_OUTPUT.extract(decoded_stderr) {
            Ok(output) => output.to_string(),
            Err(e) => {
                return Err(GdbTriageError::new(
                    GdbTriageErrorKind::Command,
                    "Could not extract child STDERR",
                    e,
                ))
            }
        };

        let backtrace_output = match MARKER_BACKTRACE.extract(decoded_stdout) {
            Ok(output) => output,
            Err(e) => {
                return Err(GdbTriageError::new(
                    GdbTriageErrorKind::Command,
                    "Failed to get triage JSON from GDB",
                    e,
                ))
            }
        };

        let backtrace_messages = match MARKER_BACKTRACE.extract(decoded_stderr) {
            Ok(output) => output,
            Err(e) => {
                return Err(GdbTriageError::new(
                    GdbTriageErrorKind::Command,
                    "Failed to get triage errors from GDB",
                    e,
                ))
            }
        };

        if backtrace_output.is_empty() && !backtrace_messages.is_empty() {
            return Err(GdbTriageError::new_detailed(
                GdbTriageErrorKind::Command,
                "Triage script emitted errors",
                backtrace_messages.lines().map(str::to_string).collect(),
            ));
        }

        match serde_json::from_str(backtrace_output) {
            Ok(json) => Ok(GdbTriageResult {
                response: json,
                child: ChildResult {
                    stdout: child_output_stdout,
                    stderr: child_output_stderr,
                    status: output.status,
                },
            }),
            Err(e) => Err(GdbTriageError::new(
                GdbTriageErrorKind::Command,
                "Failed to parse triage JSON from GDB",
                e.to_string(),
            )),
        }
    }
}