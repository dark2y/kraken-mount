//! Deliberately narrow ShadowMount v1 adapter for Playden's loopback client.
use crate::{cache, register, scan, web};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{atomic::Ordering, Arc};

const MANUAL: &str = "/data/krakenmount/playden-manual.json";
const PORT: u16 = 10101;

// Playden only submits its own FAKE78 titles. Do not accept arbitrary filesystem paths.
fn path_title(path: &str) -> Option<&str> {
    let tid = path.strip_prefix("/data/homebrew/")?;
    (tid.starts_with("FAKE78") && scan::valid_tid(tid)).then_some(tid)
}

#[derive(Clone, Default)]
pub struct ManualSources {
    pub paths: Vec<String>,
    pub excluded: Vec<String>,
}

pub fn load_manual() -> Result<ManualSources, String> {
    let data = match fs::read(MANUAL) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ManualSources::default()),
        Err(e) => return Err(format!("read {MANUAL}: {e}")),
    };
    let parsed: Value = serde_json::from_slice(&data).map_err(|_| format!("invalid {MANUAL}"))?;
    let read_paths = |v: &Value| -> Option<Vec<String>> {
        v.as_array()?.iter().map(|v| {
            let p = v.as_str()?;
            path_title(p)?;
            Some(p.to_owned())
        }).collect()
    };
    // Accept the pre-release array form too; never silently replace malformed state.
    if parsed.is_array() {
        return read_paths(&parsed).map(|paths| ManualSources { paths, excluded: Vec::new() })
            .ok_or_else(|| format!("invalid {MANUAL}"));
    }
    let paths = read_paths(&parsed["paths"]).ok_or_else(|| format!("invalid {MANUAL}"))?;
    let excluded = read_paths(&parsed["excluded"]).ok_or_else(|| format!("invalid {MANUAL}"))?;
    if paths.iter().any(|p| excluded.contains(p)) { return Err(format!("conflicting {MANUAL}")); }
    Ok(ManualSources { paths, excluded })
}

fn save_manual(manual: &ManualSources) -> std::io::Result<()> {
    let tmp = format!("{MANUAL}.tmp");
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        serde_json::to_writer(&mut f, &json!({"paths":manual.paths,"excluded":manual.excluded}))?;
        f.sync_all()?;
        fs::rename(&tmp, MANUAL)?;
        fs::File::open(crate::DATA_DIR)?.sync_all()
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

pub fn start(state: Arc<web::State>) {
    std::thread::spawn(move || {
        let mut logged = false;
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", PORT)) {
                Ok(listener) => break listener,
                Err(e) => {
                    if !logged { log!("Playden compatibility API: port {PORT} unavailable: {e}; retrying without replacing its owner"); logged = true; }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        };
        log!("Playden compatibility API on 127.0.0.1:{PORT}");
        for conn in listener.incoming().flatten() {
            let state = state.clone();
            std::thread::spawn(move || { let _ = web::handle(&state, conn, true); });
        }
    });
}

fn response(code: u16, v: Value) -> web::Reply {
    web::reply(code, "application/json", v.to_string())
}
fn error(code: u16, status: i32, message: &str) -> web::Reply {
    response(code, json!({"status":status,"error":message}))
}

pub fn route(state: &web::State, head: &str, body: &[u8]) -> web::Reply {
    let mut request = head.lines().next().unwrap_or("").split_whitespace();
    let (method, route) = (request.next().unwrap_or(""), request.next().unwrap_or(""));
    if method != "POST" { return error(404, libc::ENOENT, "not_found"); }
    let v: Value = match serde_json::from_slice(body) {
        Ok(Value::Object(obj)) => Value::Object(obj),
        _ => return error(400, libc::EINVAL, "expected JSON object"),
    };
    match route {
        "/api/v1/version" => response(200, json!({"status":0,"api_version":1,
            "shadowmount_version":format!("Kraken-Mount-{}", env!("CARGO_PKG_VERSION")),
            "capabilities":["list_games","list_manual_sources","add_manual_source","remove_manual_source","rescan","uninstall_game"]})),
        "/api/v1/games" => {
            // An incomplete initial snapshot must never be presented as authoritative absence.
            if !state.inventory_ready.load(Ordering::SeqCst) {
                return error(409, libc::EBUSY, "inventory_not_ready");
            }
            let games = state.games.lock().unwrap();
            let entries: Vec<_> = games.iter().map(|g| {
                let path = g.src.path.to_string_lossy();
                let available = match g.src.kind { scan::Kind::Folder(_) => g.src.path.is_dir(), scan::Kind::Pkg => g.src.path.is_file() };
                json!({"title_id":g.src.title_id,"title_name":g.src.title_name,"path":path,
                    "runtime_path":path,"source_type":if matches!(g.src.kind, scan::Kind::Pkg) { "pkg" } else { "folder" },
                    "managed":g.status != "duplicate", "installed":g.status == "ready",
                    "mounted":g.status == "ready", "source_available":available})
            }).collect();
            response(200, json!({"status":0,"count":entries.len(),"games":entries}))
        }
        "/api/v1/manual/list" => {
            if !state.manual_valid { return error(500, libc::EIO, "manual list unavailable"); }
            let manual = state.manual.lock().unwrap();
            response(200, json!({"status":0,"count":manual.paths.len(),"paths":manual.paths}))
        }
        "/api/v1/manual/add" | "/api/v1/manual/remove" => {
            if !state.manual_valid { return error(500, libc::EIO, "manual list unavailable"); }
            let Some(path) = v["path"].as_str().filter(|p| path_title(p).is_some()) else {
                return error(400, libc::EINVAL, "only /data/homebrew/FAKE78xxx is supported");
            };
            let add = route.ends_with("/add");
            if add {
                let tid = path_title(path).unwrap();
                let folder = Path::new(path);
                let real_dir = fs::symlink_metadata(folder).is_ok_and(|m| m.file_type().is_dir());
                let matching = fs::read(folder.join("sce_sys/param.json")).ok()
                    .and_then(|b| scan::parse_param(&b).ok()).is_some_and(|p| p.0 == tid);
                if !real_dir || !folder.join("eboot.bin").is_file() || !matching {
                    return error(400, libc::EINVAL, "source is not a matching game folder");
                }
            }
            // Serialize updates against a scan/build already in progress. A successful removal
            // suppresses default-root discovery too, until an explicit add re-enables it.
            let Ok(_work) = state.work.try_lock() else { return error(409, libc::EBUSY, "scanner_busy") };
            if add && state.games.lock().unwrap().iter().any(|g| g.src.title_id == path_title(path).unwrap() && g.src.path != Path::new(path)) {
                return error(409, libc::EEXIST, "title_collision");
            }
            let mut manual = state.manual.lock().unwrap();
            let mut next = manual.clone();
            let changed = if add {
                let changed = !next.paths.iter().any(|p| p == path) || next.excluded.iter().any(|p| p == path);
                next.excluded.retain(|p| p != path);
                if !next.paths.iter().any(|p| p == path) { next.paths.push(path.to_owned()); }
                changed
            } else {
                let changed = next.paths.iter().any(|p| p == path) || !next.excluded.iter().any(|p| p == path);
                next.paths.retain(|p| p != path);
                if !next.excluded.iter().any(|p| p == path) { next.excluded.push(path.to_owned()); }
                changed
            };
            if changed {
                if let Err(e) = save_manual(&next) {
                    log!("compat: save {MANUAL} for {path}: {e}");
                    // rename may have succeeded before the directory fsync failed.
                    if let Ok(on_disk) = load_manual() { *manual = on_disk; }
                    return error(500, libc::EIO, "manual list write failed");
                }
                *manual = next;
                state.wake();
            }
            response(200, json!({"status":0,"path":path,"present":add,"changed":changed}))
        }
        "/api/v1/scan" => { state.wake(); response(200, json!({"status":0,"queued":true})) }
        "/api/v1/games/uninstall" => {
            let Some(tid) = v["title_id"].as_str().filter(|t| t.starts_with("FAKE78") && scan::valid_tid(t)) else {
                return error(400, libc::EINVAL, "unsupported title_id");
            };
            uninstall(state, tid)
        }
        _ => error(404, libc::ENOENT, "unsupported route"),
    }
}

fn uninstall(state: &web::State, tid: &str) -> web::Reply {
    let Ok(_work) = state.work.try_lock() else { return error(409, libc::EBUSY, "scanner_busy") };
    if !state.inventory_ready.load(Ordering::SeqCst) || state.paused() {
        return error(409, libc::EBUSY, "runtime_busy");
    }
    let source = format!("/data/homebrew/{tid}");
    if state.manual.lock().unwrap().paths.iter().any(|p| p == &source) {
        return error(409, libc::EBUSY, "remove_manual_source_first");
    }
    // Refuse unless the source is truly gone, not just inaccessible. Never call the UI's
    // delete route: it removes the source and game-written cache files.
    match fs::symlink_metadata(&source) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        _ => return error(409, libc::EBUSY, "source_must_be_retired_first"),
    }
    {
        let games = state.games.lock().unwrap();
        if games.iter().any(|g| g.src.title_id == tid && g.src.path != Path::new(&source)) {
            return error(409, libc::EEXIST, "title_collision");
        }
        if !games.iter().any(|g| g.src.title_id == tid && g.src.path == Path::new(&source) && g.status == "ready") {
            return error(404, libc::ENOENT, "not_a_current_managed_title");
        }
    }
    // A built cache binds this exact title to this source; a native/foreign install is not ours.
    let stamp = cache::dir(tid).join(cache::DONE);
    let owned = fs::read(&stamp).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["kind"] == "folder" && v["source"] == source);
    if !owned { return error(404, libc::ENOENT, "not_a_managed_title"); }
    if let Err(e) = register::unmount_title(tid) {
        log!("{tid}: compat unmount {source}: {e}");
        return error(409, libc::EBUSY, "unmount_failed");
    }
    if let Err(e) = register::uninstall(tid) {
        log!("{tid}: compat uninstall {source}: {e}");
        return error(500, libc::EIO, "uninstall_failed");
    }
    state.titles.lock().unwrap().remove(tid);
    state.games.lock().unwrap().retain(|g| !(g.src.title_id == tid && g.src.path == Path::new(&source)));
    state.wake();
    response(200, json!({"status":0,"title_id":tid}))
}
