use std::sync::Arc;

use crate::{config, content_index, ipc};

/// Handle CLI flags. Returns true if a flag was handled and the process should exit.
pub fn handle_cli_args() -> bool {
    let args: Vec<String> = std::env::args().collect();

    // `portunus ext …` - extension developer subcommands.
    if args.get(1).map(String::as_str) == Some("ext") {
        std::process::exit(crate::cli_ext::run(&args[2..]));
    }

    // `portunus native-host …` - browser native-messaging shim for the
    // extension message bus (spawned by the browser, or `install` by the user).
    if args.get(1).map(String::as_str) == Some("native-host") {
        std::process::exit(crate::native_host::run(&args[2..]));
    }

    // `portunus render-office <file> [section]` - dump the rendered HTML for one
    // office document to stdout. Development aid: unit tests cannot judge visual
    // fidelity, so this is how a renderer gets compared against LibreOffice
    // (`portunus render-office x.xlsx > /tmp/x.html` and open it in a browser).
    if args.get(1).map(String::as_str) == Some("render-office") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: portunus render-office <file> [section] [term…]");
            std::process::exit(2);
        };
        let section = args.get(3).and_then(|s| s.parse::<u32>().ok());
        // Anything after the section is a search term, so the emit-time
        // highlighting can be inspected the way the preview would show it.
        let terms: Vec<String> = args
            .iter()
            .skip(if section.is_some() { 4 } else { 3 })
            .cloned()
            .collect();
        match crate::office::render(path, section, &terms) {
            Ok(doc) => {
                // Notes and section list go to stderr so stdout stays pipeable
                // straight into a file.
                eprintln!(
                    "shape={:?} section={}/{} truncated={} natural={:?} page={:?}",
                    doc.shape,
                    doc.section,
                    doc.sections.len(),
                    doc.truncated,
                    doc.natural,
                    doc.page
                );
                for (i, name) in doc.sections.iter().enumerate() {
                    eprintln!("  [{i}] {name}");
                }
                for n in &doc.notes {
                    eprintln!("note: {n}");
                }
                eprintln!(
                    "marks={} best={:?}",
                    doc.html.matches("<mark class=\"preview-hl\"").count(),
                    doc.best_mark_id
                );
                println!(
                    "<!doctype html><meta charset=\"utf-8\"><title>{}</title>{}",
                    path, doc.html
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("portunus: {e}");
                std::process::exit(1);
            }
        }
    }

    // `portunus index-office <file>` - dump what the content index would store for
    // one office document: one block per section, in the renderer's section order.
    // The companion of `render-office`: that one shows what the reader sees, this
    // one what a search can find, and a mismatch between their section counts is
    // exactly the bug that opens a preview on the wrong sheet.
    if args.get(1).map(String::as_str) == Some("index-office") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: portunus index-office <file>");
            std::process::exit(2);
        };
        match crate::office::extract_office_text(path) {
            Ok(text) => {
                let sections: Vec<&str> = text.split(crate::office::SECTION_SEP).collect();
                let rendered = crate::office::render(path, None, &[])
                    .map(|d| d.sections.len().max(1))
                    .unwrap_or(0);
                eprintln!(
                    "sections={} rendered_sections={} chars={}",
                    sections.len(),
                    rendered,
                    text.chars().count()
                );
                for (i, s) in sections.iter().enumerate() {
                    println!("── section {i} ({} chars) ──", s.chars().count());
                    println!("{s}");
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("portunus: {e}");
                std::process::exit(1);
            }
        }
    }

    // --reload-extension <name>: targeted hot-reload of one extension.
    if let Some(pos) = args.iter().position(|a| a == "--reload-extension") {
        let Some(name) = args.get(pos + 1) else {
            eprintln!("usage: portunus --reload-extension <name>");
            std::process::exit(2);
        };
        if !ipc::try_signal_running(&format!("reload-extension:{name}")) {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }

    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("portunus {}", env!("CARGO_PKG_VERSION"));
        return true;
    }

    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("portunus {}: application launcher and power-user search for Linux

USAGE:
  portunus [FLAG]

With no flags: start Portunus, or show the window of the already-running instance.

FLAGS:
  --show              Show the launcher window (signals running instance)
  --close             Close the launcher window (signals running instance)
  --toggle            Toggle the launcher window (signals running instance)
  --clipboard         Show the launcher pre-filled with \"clipboard\"
  --reindex           Rebuild the content index and re-walk the file roots
  --reload-config     Reload config from file without restarting
  --reload-extensions Re-discover and reload WASM extensions (picks up rebuilt wasm)
  --reload-extension <name>
                      Reload a single extension (used by `portunus ext dev`)
  --reload-theme      Re-read the external matugen.css theme (matugen post_hook)
  --version, -V       Print version and exit
  --help, -h          Show this help message

SUBCOMMANDS:
  ext new <name>      Scaffold a new extension project
  ext dev <dir>       Link a working dir into Portunus + auto-reload on rebuild
  ext validate <dir>  Check an extension's manifest and wasm exports
  ext pack <dir>      Build a distributable .portext archive
  native-host <name>  Relay browser native messaging to the extension message
                      bus (normally spawned by the browser, not by hand)
  native-host install <name> --ff-ext-id <id@domain>
                      Write the wrapper script + Firefox manifest for <name>
  render-office <file> [section]
                      Print an office document's rendered preview HTML to stdout
                      (development aid for comparing fidelity against LibreOffice)
  index-office <file> Print the per-section text the content index would store", env!("CARGO_PKG_VERSION"));
        return true;
    }

    if std::env::args().any(|a| a == "--show") {
        if !ipc::try_signal_running("show") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--close") {
        if !ipc::try_signal_running("close") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--toggle") {
        if !ipc::try_signal_running("toggle") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--clipboard") {
        if !ipc::try_signal_running("show:clipboard ") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--reload-config") {
        if !ipc::try_signal_running("reload-config") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--reload-extensions") {
        if !ipc::try_signal_running("reload-extensions") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--reload-theme") {
        if !ipc::try_signal_running("reload-theme") {
            eprintln!("portunus: no running instance found");
            std::process::exit(1);
        }
        return true;
    }
    if std::env::args().any(|a| a == "--reindex") {
        if !ipc::try_signal_running("reindex") {
            // No running instance - run standalone with stderr progress.
            let cfg = config::Config::load();
            if cfg.content.enabled {
                match content_index::ContentIndex::open() {
                    Ok(index) => {
                        let index = Arc::new(index);
                        index.clear().ok();
                        content_index::run_content_indexer(
                            Arc::clone(&index),
                            &cfg.content,
                            Some(Arc::new(|indexed, total| {
                                eprint!("\r[content] {indexed}/{total}");
                                if indexed >= total {
                                    eprintln!();
                                }
                            })),
                        );
                        eprintln!("[content] reindex complete");
                    }
                    Err(e) => eprintln!("[content] failed to open index: {e}"),
                }
            } else {
                eprintln!("[content] content indexing is disabled in config");
            }
        }
        return true;
    }

    // Bare `portunus` with an instance already running: surface that window instead
    // of starting a second process that would fight over the IPC socket, frecency
    // db, and content index. A stale socket file refuses the connect, so this
    // falls through to a normal startup.
    if args.len() == 1 && ipc::try_signal_running("show") {
        eprintln!("portunus: already running - showing existing window");
        return true;
    }

    false
}
