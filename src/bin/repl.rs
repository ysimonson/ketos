extern crate getopts;
extern crate ketos;
extern crate libc;

use std::env::{split_paths, var_os};
use std::io::{stderr, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use getopts::{Options, ParsingStyle};
use ketos::{Builder, Interpreter, Error, ParseErrorKind, RestrictConfig, take_traceback};

mod completion;
mod readline;

fn main() {
    let status = run();
    std::process::exit(status);
}

fn run() -> i32 {
    let args = std::env::args().collect::<Vec<_>>();
    let mut opts = Options::new();

    // Allow arguments that appear to be options to be passed to scripts
    opts.parsing_style(ParsingStyle::StopAtFirstFree);

    opts.optopt  ("e", "", "Evaluate one expression and exit", "EXPR");
    opts.optflag ("h", "help", "Print this help message and exit");
    opts.optflag ("i", "interactive", "Run interactively even with a file");
    opts.optmulti("I", "", "Add DIR to list of module search paths", "DIR");
    opts.optopt  ("R", "restrict", "Configure execution restrictions; \
                                    see `-R help` for more details", "SPEC");
    opts.optflag ("", "no-rc", "Do not run ~/.ketosrc.ket on startup");
    opts.optflag ("V", "version", "Print version and exit");

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(e) => {
            let _ = writeln!(stderr(), "{}: {}", args[0], e);
            return 1;
        }
    };

    if matches.opt_present("version") {
        print_version();
        return 0;
    }
    if matches.opt_present("help") {
        print_usage(&args[0], &opts);
        return 0;
    }

    // Search current directory first
    let mut paths = vec![PathBuf::new()];

    if let Some(p) = var_os("KETOS_PATH") {
        paths.extend(split_paths(&p));
    }

    paths.extend(matches.opt_strs("I").into_iter().map(PathBuf::from));

    let mut builder = Builder::new()
        .search_paths(paths);

    if let Some(res) = matches.opt_str("restrict") {
        if res == "help" {
            print_restrict_usage();
            return 0;
        }

        builder = match parse_restrict(&res) {
            Ok(res) => builder.restrict(res),
            Err(e) => {
                println!("{}: {}", args[0], e);
                return 1;
            }
        }
    }

    let interp = builder.finish();

    let interactive = matches.opt_present("interactive") ||
        (matches.free.is_empty() && !matches.opt_present("e"));

    if let Some(expr) = matches.opt_str("e") {
        if !run_expr(&interp, &expr) && !interactive {
            return 1;
        }
    } else if !matches.free.is_empty() {
        interp.set_args(&matches.free[1..]);
        if !run_file(&interp, Path::new(&matches.free[0])) && !interactive {
            return 1;
        }
    }

    if interactive {
        if !matches.opt_present("no-rc") {
            if let Some(p) = std::env::home_dir() {
                let rc = p.join(".ketosrc.ket");
                if rc.is_file() {
                    // Ignore error in interactive mode
                    run_file(&interp, &rc);
                }
            }
        }

        run_repl(&interp);
    }

    0
}

fn parse_restrict(params: &str) -> Result<RestrictConfig, String> {
    let mut res = RestrictConfig::permissive();

    for param in params.split(',') {
        match param {
            "permissive" => res = RestrictConfig::permissive(),
            "strict" => res = RestrictConfig::strict(),
            _ => {
                let (name, value) = match param.find('=') {
                    Some(pos) => (&param[..pos], &param[pos + 1..]),
                    None => return Err(format!("unrecognized restrict option: {}", param))
                };

                match name {
                    "execution_time" =>
                        res.execution_time = Some(Duration::from_millis(
                            try!(parse_param(name, value)))),
                    "call_stack_size" =>
                        res.call_stack_size = try!(parse_param(name, value)),
                    "value_stack_size" =>
                        res.value_stack_size = try!(parse_param(name, value)),
                    "namespace_size" =>
                        res.namespace_size = try!(parse_param(name, value)),
                    "memory_limit" =>
                        res.memory_limit = try!(parse_param(name, value)),
                    "max_integer_size" =>
                        res.max_integer_size = try!(parse_param(name, value)),
                    "max_syntax_nesting" =>
                        res.max_syntax_nesting = try!(parse_param(name, value)),
                    _ => return Err(format!("unrecognized parameter: {}", name))
                }
            }
        }
    }

    Ok(res)
}

fn parse_param<T: FromStr>(name: &str, value: &str) -> Result<T, String> {
    value.parse().map_err(|_| format!("invalid `{}` value: {}", name, value))
}

fn display_error(interp: &Interpreter, e: &Error) {
    if let Some(trace) = take_traceback() {
        interp.display_trace(&trace);
    }
    interp.display_error(e);
}

fn run_expr(interp: &Interpreter, expr: &str) -> bool {
    match interp.run_single_expr(expr, None) {
        Ok(value) => {
            interp.display_value(&value);
            true
        }
        Err(e) => {
            display_error(&interp, &e);
            false
        }
    }
}

fn run_file(interp: &Interpreter, file: &Path) -> bool {
    match interp.run_file(file) {
        Ok(()) => true,
        Err(e) => {
            display_error(&interp, &e);
            false
        }
    }
}

#[derive(Copy, Clone)]
enum Prompt {
    Normal,
    OpenComment,
    OpenParen,
    OpenString,
    DocComment,
}

fn read_line(interp: &Interpreter, prompt: Prompt) -> Option<String> {
    let prompt = match prompt {
        Prompt::Normal => "ketos=> ",
        Prompt::OpenComment => "ketos#> ",
        Prompt::OpenParen => "ketos(> ",
        Prompt::OpenString => "ketos\"> ",
        Prompt::DocComment => "ketos;> ",
    };

    readline::read_line(prompt, interp.scope())
}

fn run_repl(interp: &Interpreter) {
    let mut buf = String::new();
    let mut prompt = Prompt::Normal;

    while let Some(line) = read_line(interp, prompt) {
        if line.chars().all(|c| c.is_whitespace()) {
            continue;
        }

        readline::push_history(&line);
        buf.push_str(&line);
        buf.push('\n');

        match interp.compile_exprs(&buf) {
            Ok(code) => {
                prompt = Prompt::Normal;
                if !code.is_empty() {
                    match interp.execute_program(code) {
                        Ok(v) => interp.display_value(&v),
                        Err(e) => display_error(&interp, &e)
                    }
                }
            }
            Err(Error::ParseError(ref e)) if e.kind == ParseErrorKind::MissingCloseParen => {
                prompt = Prompt::OpenParen;
                continue;
            }
            Err(Error::ParseError(ref e)) if e.kind == ParseErrorKind::UnterminatedComment => {
                prompt = Prompt::OpenComment;
                continue;
            }
            Err(Error::ParseError(ref e)) if e.kind == ParseErrorKind::UnterminatedString => {
                prompt = Prompt::OpenString;
                continue;
            }
            Err(Error::ParseError(ref e)) if e.kind == ParseErrorKind::DocCommentEof => {
                prompt = Prompt::DocComment;
                continue;
            }
            Err(ref e) => {
                prompt = Prompt::Normal;
                display_error(&interp, e);
            }
        }

        buf.clear();
        interp.clear_codemap();
    }

    println!("");
}

fn print_version() {
    println!("ketos {}", version());
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn print_usage(arg0: &str, opts: &Options) {
    print!("{}", opts.usage(&format!("Usage: {} [OPTIONS] [FILE] [ARGS]", arg0)));
}

fn print_restrict_usage() {
    print!(
r#"The `-R` / `--restrict` option accepts a comma-separated list of parameters:

  permissive
    Applies "permissive" restrictions (default)

  strict
    Applies "strict" restrictions

  key=value
    Assigns a value to the named restriction configuration parameter.
    Accepted keys are:

      execution_time          Maximum execution time, in milliseconds
      call_stack_size         Maximum call frames
      value_stack_size        Maximum values stored on the stack
      namespace_size          Maximum values stored in global namespace
      memory_limit            Maximum total held memory, in abstract units
      max_integer_size        Maximum integer size, in bits
      max_syntax_nesting      Maximum nested syntax elements
"#);
}
