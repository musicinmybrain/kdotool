use std::io::Write;
use std::process::Command;
use std::time::Duration;
use phf::phf_map;
use dbus::blocking::Connection;
use tempfile::NamedTempFile;
use handlebars::Handlebars;
use serde_json::json;
use lexopt::Parser;

const SCRIPT_HEADER: &str = r#"
print("{{{marker}}} START");

function output_debug(message) {
    {{#if debug}}
    print("{{{marker}}} DEBUG", message);
    {{/if}}
}

function output_error(message) {
    print("{{{marker}}} ERROR", message);
}

function output_result(message) {
    print("{{{marker}}} RESULT", message);
}

function run() {
    var window_stack = [];
"#;

const SCRIPT_FOOTER: &str = r#"
}

run();

print("{{{marker}}} FINISH");
"#;

const STEP_SEARCH : &str = r#"
    output_debug("STEP search {{{search_term}}}")
    const re = new RegExp("{{{search_term}}}", "i");
    {{#if kde5}}
    t = workspace.clientList();
    {{else}}
    t = workspace.windowList();
    {{/if}}
    window_stack = [];
    for (var i=0; i<t.length; i++) {
        var w = t[i];
        var candidates = [w.caption, w.resourceClass, w.resourceName, w.windowRole,];
        output_debug(candidates)
        {{#if match_any}}
        for (var j=0; j<candidates.length; j++) {
            if (candidates[j].search(re) >= 0) {
                window_stack.push(w);
                break;
            }
        }
        {{else}}
        var mismatch = false;
        for (var j=0; j<candidates.length; j++) {
            if (candidates[j].search(re) < 0) {
                mismatch = true;
                break;
            }
        }
        if (!mismatch) {
            window_stack.push(w);
        }
        {{/if}}
    }
"#;

const STEP_GETACTIVEWINDOW : &str = r#"
    output_debug("STEP getactivewindow")
    window_stack = [workspace.activeWindow];
"#;

const STEP_ACTION_ON_WINDOW_ID : &str = r#"
    output_debug("STEP {{{step_name}}}")
    {{#if kde5}}
    t = workspace.clientList();
    {{else}}
    t = workspace.windowList();
    {{/if}}
    for (var i=0; i<t.length; i++) {
        var w = t[i];
        if (w.internalId == "{{{window_id}}}") {
            {{{action}}}
            break;
        }
    }
"#;

const STEP_ACTION_ON_STACK_ITEM : &str = r#"
    output_debug("STEP {{{step_name}}}")
    if (window_stack.length > 0) {
        if ({{{item_index}}} > window_stack.length || {{{item_index}}} < 1) {
            output_error("Invalid window stack selection '{{{item_index}}}' (out of range)");
        } else {
            var w = window_stack[{{{item_index}}}-1];
            {{{action}}}
        }
    }
"#;

const STEP_ACTION_ON_STACK_ALL : &str = r#"
    output_debug("STEP {{{step_name}}}")
    for (var i=0; i<window_stack.length; i++) {
        var w = window_stack[i];
        {{{action}}}
    }
"#;

const STEP_LAST_OUTPUT : &str = r#"
    for (var i = 0; i < window_stack.length; ++i) {
        output_result(window_stack[i].internalId);
    }
"#;

static ACTIONS: phf::Map<&'static str, &'static str> = phf_map! {
    "getwindowname" => "output_result(w.caption);",
    "getwindowclassname" => "output_result(w.resourceClass);",
    "getwindowgeometry" => "output_result(`Window ${w.internalId}`); output_result(`  Position: ${w.x},${w.y}`); output_result(`  Geometry: ${w.width}x${w.height}`);",
    "getwindowpid" => "output_result(w.pid);",
    "windowminimize" => "w.minimized = true;",
    "windowraise" => "workspace.raiseWindow(w);",
    "windowclose" => "w.closeWindow();",
    "windowkill" => "w.killWindow();",
    "windowactivate" => "workspace.setActiveWindow(w);",
};

struct Context {
    cmdline: Box<Parser>,
    debug: bool,
    dry_run: bool,
    kde5: bool,
    marker: String,
}

fn next_arg_is_option(cmdline : &mut Parser) -> bool {
    match cmdline.try_raw_args().unwrap().peek() {
        Some(arg) => {
            return arg.to_string_lossy().starts_with("-");
        },
        None => {
            return false;
        }
    }
}

fn generate_script(context : &mut Context) -> anyhow::Result<String> {
    use lexopt::prelude::*;

    let mut result = String::new();
    let reg = Handlebars::new();
    let render_context = json!({
        "marker": context.marker,
        "kde5": context.kde5,
        "debug": context.debug
    });

    result.push_str(&reg.render_template(SCRIPT_HEADER, &render_context)?);

    let mut last_step_is_query = false;

    while let Some(arg) = context.cmdline.next()? {
        match arg {
            Value(val) => {
                let command : String = val.to_string_lossy().into();
                match command.as_ref() {

                    "search" => {
                        let arg = context.cmdline.next()?.unwrap();
                        match arg {
                            Value(val) => {
                                let search_term : String = val.to_string_lossy().into();
                                result.push_str(&reg.render_template(STEP_SEARCH, &json!({"search_term": search_term, "match_any": false}))?);
                                last_step_is_query = true;
                            },
                            _ => {
                                return Err(anyhow::anyhow!("Missing search term"));
                            }
                        }
                    },

                    "getactivewindow" => {
                        result.push_str(&reg.render_template(STEP_GETACTIVEWINDOW, &render_context)?);
                        last_step_is_query = true;
                    },

                    _ => {
                        if ACTIONS.contains_key(command.as_ref()) {
                            let mut arg1 = "%1".to_string();
                            while next_arg_is_option(&mut context.cmdline) {
                                let arg = context.cmdline.next()?.unwrap();
                                match arg {
                                    Value(val) => {
                                        arg1 = val.to_string_lossy().into();
                                    },
                                    _ => {
                                        return Err(anyhow::anyhow!("Unexpected option"));
                                    }
                                }
                            }

                            let action = &reg.render_template(ACTIONS.get(command.as_ref()).unwrap(), &render_context)?;
                            if arg1 == "%@" {
                                result.push_str(&reg.render_template(STEP_ACTION_ON_STACK_ALL, &json!({"step_name": command, "action": action}))?);
                            } else if arg1.starts_with("%") {
                                let index = arg1[1..].parse::<i32>()?;
                                result.push_str(&reg.render_template(STEP_ACTION_ON_STACK_ITEM, &json!({"step_name": command, "action": action, "item_index": index}))?);
                            } else {
                                result.push_str(&reg.render_template(STEP_ACTION_ON_WINDOW_ID, &json!({"step_name": command, "action": action, "window_id": arg1}))?);
                            }

                            last_step_is_query = false;
                        } else {
                            return Err(anyhow::anyhow!("Unknown command: {}", command));
                        }
                    }
                }
            },
            _ => {
                return Err(anyhow::anyhow!("Unexpected option"));
            }
        }
    }

    if last_step_is_query {
        result.push_str(&reg.render_template(STEP_LAST_OUTPUT, &render_context)?);
    }

    result.push_str(&reg.render_template(SCRIPT_FOOTER, &render_context)?);

    Ok(result)
}

fn main() -> anyhow::Result<()> {
    use lexopt::prelude::*;

    env_logger::init();

    let mut context = Context {
        cmdline: Box::new(Parser::from_env()),
        debug: false,
        dry_run: false,
        kde5: false,
        marker: String::new(),
    };

    match std::env::var("KDE_SESSION_VERSION") {
        Ok(version) => {
            if version == "5" {
                context.kde5 = true;
            }
        },
        Err(_) => {},
    }

    // Parse global options
    if context.cmdline.try_raw_args().unwrap().peek().is_none() {
        help();
        return Ok(());
    }

    while next_arg_is_option(&mut context.cmdline) {
        let arg = context.cmdline.next()?.unwrap();
        match arg {
            Short('h') | Long("help") => {
                help();
                return Ok(());
            },
            Short('d') | Long("debug") => {
                context.debug = true;
            },
            Short('n') | Long("dry-run") => {
                context.dry_run = true;
            },
            _ => {
                return Err(arg.unexpected().into());
            }
        }
    }

    log::debug!("===== Generate KWin script =====");
    let mut script_file = NamedTempFile::with_prefix("kdotool-")?;
    context.marker = script_file.path().file_name().unwrap().to_str().unwrap().to_string();

    let script_contents = generate_script(&mut context)?;

    log::debug!("Script:{}", script_contents);
    script_file.write_all(script_contents.as_bytes())?;
    let script_file_path = script_file.into_temp_path();

    log::debug!("===== Load script into KWin =====");
    let conn = Connection::new_session()?;
    let kwin_proxy = conn.with_proxy("org.kde.KWin", "/Scripting", Duration::from_millis(5000));
    let (script_id,): (i32,) = kwin_proxy.method_call("org.kde.kwin.Scripting", "loadScript", (script_file_path.to_str().unwrap(),))?;
    log::debug!("Script ID: {}", script_id);

    log::debug!("===== Run script =====");
    let script_proxy = conn.with_proxy("org.kde.KWin", format!("/Scripting/Script{}", script_id), Duration::from_millis(5000));
    let start_time = chrono::Local::now();
    script_proxy.method_call("org.kde.kwin.Script", "run", ())?;
    script_proxy.method_call("org.kde.kwin.Script", "stop", ())?;

    let journal = Command::new("journalctl")
        .arg(format!("--since={}", start_time.format("%Y-%m-%d %H:%M:%S")))
        .arg("--user")
        .arg("--unit=plasma-kwin_wayland.service")
        .arg("--unit=plasma-kwin_x11.service")
        .arg("--output=cat")
        .output()?;
    let output = String::from_utf8(journal.stdout)?;
    log::debug!("KWin log from the systemd journal:\n{}", output.trim_end());

    log::debug!("===== Output =====");
    let script_marker = &format!("js: {} ", script_file_path.file_name().unwrap().to_str().unwrap());
    for line in output.lines() {
        if line.starts_with(script_marker) {
            let t = &line[script_marker.len()..];
            const RESULT: &str = "RESULT ";
            const ERROR: &str = "ERROR ";
            if t.starts_with(RESULT) {
                println!("{}", &t[RESULT.len()..]);
            } else if t.starts_with(ERROR) {
                eprintln!("{}", &t[ERROR.len()..]);
            }
        }
    }

    Ok(())
}

fn help() {
    println!("Usage: kdotool [options] <command> [args...]");
    println!();
    println!("Options:");
    println!("  -h, --help       Show this help");
    println!("  -d, --debug      Enable debug output");
    println!("  -n, --dry-run    Don't actually run the script. Just print it to stdout.");
    println!();
    println!("Commands:");
    println!("  search <term>");
    println!("  getactivewindow");
    println!("  getwindowname <window>");
    println!("  getwindowclassname <window>");
    println!("  getwindowgeometry <window>");
    println!("  getwindowpid <window>");
    println!("  windowminimize <window>");
    println!("  windowraise <window>");
    println!("  windowclose <window>");
    println!("  windowkill <window>");
    println!("  windowactivate <window>");
    println!();
    println!("Window can be specified as:");
    println!("  %1 - the first window in the stack (default)");
    println!("  %2 - the second window in the stack");
    println!("  %@ - all windows in the stack");
    println!("  <window id> - the window with the given ID");
}
