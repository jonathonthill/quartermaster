//! `archive-helper files`: the protocol on a file server (an analysis server,
//! or the archive server's own disks). It browses and searches ordinary files
//! and runs transfer jobs that send to or retrieve from an archive.

use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::proto::{self, ErrorKind, HelloInfo, Kind, Request, Response};
use crate::{copy, fsview, jobs, keys, link, trash};

pub struct FilesOptions {
    /// The folder to show first.
    pub start: PathBuf,
    pub helper_version: String,
    /// Start a recorded job running in the background.
    pub start_job: Box<dyn FnMut(&str) + Send>,
    /// This helper's executable, which ssh runs to ask sign-in questions.
    pub askpass: PathBuf,
}

fn respond(w: &mut impl Write, r: &Response) -> std::io::Result<()> {
    proto::write_json(w, Kind::Msg, r)?;
    w.flush()
}

fn text_err(msg: String) -> Response {
    Response::Error { kind: ErrorKind::Other, message: msg }
}

pub fn serve_files(input: impl Read, output: impl Write, mut opts: FilesOptions) -> Result<()> {
    let mut r = BufReader::new(input);
    let mut w = BufWriter::new(output);
    let mut buf = Vec::new();
    let mut greeted = false;
    loop {
        let kind = match proto::read_frame(&mut r, &mut buf)? {
            None => return Ok(()),
            Some(k) => k,
        };
        if kind != Kind::Msg {
            return Err(Error::other("protocol error: expected a request"));
        }
        let req: Request = proto::parse_json(&buf)?;
        if !greeted && !matches!(req, Request::Hello { .. }) {
            respond(&mut w, &text_err("say hello first".into()))?;
            continue;
        }
        let resp = match req {
            Request::Hello { version, .. } => {
                if version != proto::PROTOCOL_VERSION {
                    respond(
                        &mut w,
                        &text_err(format!("helper speaks protocol {}, client {version}; update the helper", proto::PROTOCOL_VERSION)),
                    )?;
                    return Ok(());
                }
                greeted = true;
                Response::Hello(HelloInfo::here(opts.start.display().to_string(), None, false, opts.helper_version.clone()))
            }
            Request::FsList { path, show_hidden } => {
                let p = path.unwrap_or_else(|| opts.start.display().to_string());
                match fsview::list(Some(&p), show_hidden) {
                    Ok(l) => Response::Listing(l),
                    Err(e) => text_err(e),
                }
            }
            Request::FsSearch { root, query, every_file, show_hidden } => {
                match fsview::search(&root, &query, show_hidden, every_file, &|| false) {
                    Ok(f) => Response::Found(f),
                    Err(e) => text_err(e),
                }
            }
            Request::FsMeasure { paths } => Response::Measure(fsview::measure(&paths)),
            Request::FsPlaces => Response::Places(fsview::places()),
            Request::FsMkdir { path } => match std::fs::create_dir(&path) {
                Ok(()) => Response::Ok,
                Err(e) => text_err(format!("Couldn't create the folder: {e}")),
            },
            Request::FsStat { path } => match fsview::typed_path(&path) {
                Ok(p) => match copy::stat_path(&p, false) {
                    Ok(s) => Response::Stat(Some(s)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Response::Stat(None),
                    Err(e) => text_err(format!("Can't read {}: {e}", p.display())),
                },
                Err(e) => text_err(e),
            },
            Request::FsWalk { root } => match fsview::typed_path(&root).map_err(Error::other).and_then(|p| copy::walk_local(&p)) {
                Ok(w) => Response::Walked(w),
                Err(e) => Response::from_error(&e),
            },
            Request::FsMkdirs { paths } => {
                let made = paths.iter().try_for_each(|p| {
                    let p = fsview::typed_path(p).map_err(Error::other)?;
                    fs::create_dir_all(&p).map_err(|e| Error::other(format!("Couldn't create the folder {}: {e}", p.display())))
                });
                match made {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::from_error(&e),
                }
            }
            Request::FsRemove { path } => match fsview::typed_path(&path) {
                Ok(p) => match copy::remove_path(&p) {
                    Ok(()) => Response::Ok,
                    Err(e) => text_err(format!("Couldn't remove {}: {e}", p.display())),
                },
                Err(e) => text_err(e),
            },
            Request::FsTrash { paths } => {
                let parsed: std::result::Result<Vec<PathBuf>, String> = paths.iter().map(|p| fsview::typed_path(p)).collect();
                match parsed.map_err(Error::other).and_then(|ps| trash::send_in(&trash::default_dir(), &fsview::home(), &ps)) {
                    Ok(report) => Response::TrashReport(report),
                    Err(e) => Response::from_error(&e),
                }
            }
            Request::FsTrashList => Response::Trashed(trash::list_in(&trash::default_dir())),
            Request::FsTrashRestore { id } => match trash::restore_in(&trash::default_dir(), &id, None) {
                Ok(path) => Response::Text(path),
                Err(e) => Response::from_error(&e),
            },
            Request::FsTrashEmpty { ids } => match trash::empty_in(&trash::default_dir(), &fsview::home(), ids.as_deref()) {
                Ok(n) => Response::Id(n as i64),
                Err(e) => Response::from_error(&e),
            },
            Request::FsDelete { paths } => {
                let parsed: std::result::Result<Vec<PathBuf>, String> = paths.iter().map(|p| fsview::typed_path(p)).collect();
                match parsed.and_then(|ps| fsview::delete_paths_in(&fsview::home(), &ps)) {
                    Ok(n) => Response::Id(n as i64),
                    Err(e) => text_err(e),
                }
            }
            Request::FsRead { path, offset } => {
                match fsview::typed_path(&path).map_err(Error::other).and_then(|p| copy::stat_path(&p, true).map(|s| (p, s)).map_err(Error::from)) {
                    Ok((p, stat)) => {
                        respond(&mut w, &Response::FsStart { size: stat.size })?;
                        let sent = copy::read_stream(&p, offset, &mut |chunk| {
                            proto::write_frame(&mut w, Kind::Data, chunk).map_err(Error::from)
                        });
                        match sent {
                            Ok(digest) => proto::write_json(&mut w, Kind::End, &proto::Trailer { digest: Some(digest) })?,
                            Err(e) => proto::write_frame(&mut w, Kind::Abort, e.to_string().as_bytes())?,
                        }
                        w.flush()?;
                        continue;
                    }
                    Err(e) => Response::from_error(&e),
                }
            }
            Request::FsWrite { path, size, mtime_ns, mode, offset } => {
                let began = fsview::typed_path(&path)
                    .map_err(Error::other)
                    .and_then(|p| copy::PartWriter::begin(&p, size, mtime_ns, mode, offset));
                match began {
                    Ok(mut part) => {
                        respond(&mut w, &Response::Proceed)?;
                        // Read the stream through to its end, so both sides stay in step.
                        let mut failed: Option<Error> = None;
                        let outcome = loop {
                            match proto::read_frame(&mut r, &mut buf)? {
                                Some(Kind::Data) => {
                                    if failed.is_none() {
                                        if let Err(e) = part.write(&buf) {
                                            failed = Some(e);
                                        }
                                    }
                                }
                                Some(Kind::End) => {
                                    let trailer: proto::Trailer = proto::parse_json(&buf)?;
                                    break match (failed.take(), trailer.digest) {
                                        (Some(e), _) => Err(e),
                                        (None, Some(d)) => part.finish(d),
                                        (None, None) => Err(Error::other("no checksum was sent")),
                                    };
                                }
                                Some(Kind::Abort) => break Err(failed.take().unwrap_or_else(|| Error::other("the upload was cancelled"))),
                                // A lost connection keeps the part file (already on disk) for a later resume.
                                _ => return Ok(()),
                            }
                        };
                        match outcome {
                            Ok(()) => Response::Ok,
                            Err(e) => Response::from_error(&e),
                        }
                    }
                    Err(e) => Response::from_error(&e),
                }
            }
            Request::JobStart { spec } => match jobs::create(&spec) {
                Ok(id) => {
                    // A relayed job is started by the computer relaying it.
                    if jobs::starts_by_itself(&spec) {
                        (opts.start_job)(&id);
                    }
                    Response::Text(id)
                }
                Err(e) => Response::from_error(&e),
            },
            Request::JobList { resume_interrupted } => {
                if resume_interrupted {
                    for id in jobs::interrupted() {
                        (opts.start_job)(&id);
                    }
                    // Give restarted jobs a moment to take their locks.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                Response::Jobs(jobs::list())
            }
            Request::JobCancel { id } => match jobs::cancel(&id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::from_error(&e),
            },
            Request::JobPause { id } => match jobs::pause(&id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::from_error(&e),
            },
            Request::JobDiscard { id } => match jobs::discard(&id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::from_error(&e),
            },
            Request::JobResume { id } => match jobs::resume(&id) {
                Ok(start) => {
                    if start {
                        (opts.start_job)(&id);
                    }
                    Response::Ok
                }
                Err(e) => Response::from_error(&e),
            },
            Request::TransferKey => match keys::ensure_keypair() {
                Ok(k) => Response::Text(k),
                Err(e) => Response::from_error(&e),
            },
            Request::TrustHosts { lines } => match keys::trust_hosts(&lines) {
                Ok(()) => Response::Ok,
                Err(e) => Response::from_error(&e),
            },
            Request::RouteTest { target } => match jobs::route_test(&target) {
                Ok(h) => Response::Hello(h),
                Err(e) => Response::from_error(&e),
            },
            Request::SignIn { target } => {
                // Relay each of ssh's questions to the client and wait for its answer.
                let mut next = 0u64;
                let mut ask = |text: &str| -> Option<String> {
                    next += 1;
                    respond(&mut w, &Response::Prompt { id: next, text: text.to_string() }).ok()?;
                    match proto::read_frame(&mut r, &mut buf) {
                        Ok(Some(Kind::Msg)) => match proto::parse_json::<Request>(&buf) {
                            Ok(Request::PromptAnswer { id, answer }) if id == next => answer,
                            _ => None,
                        },
                        _ => None,
                    }
                };
                match link::open(&target, &opts.askpass, &mut ask) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::from_error(&e),
                }
            }
            Request::LinkCheck { target } => Response::Bools(vec![link::check(&target)]),
            Request::SignOut { target } => match link::close(&target) {
                Ok(()) => Response::Ok,
                Err(e) => Response::from_error(&e),
            },
            _ => text_err("that request is for archives, not file servers".into()),
        };
        respond(&mut w, &resp)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_over_the_protocol() {
        let mut input = Vec::new();
        let hello = Request::Hello { version: proto::PROTOCOL_VERSION, client: "test".into() };
        proto::write_json(&mut input, Kind::Msg, &hello).unwrap();
        proto::write_json(&mut input, Kind::Msg, &Request::FsPlaces).unwrap();
        let opts =
            FilesOptions { start: std::env::temp_dir(), helper_version: "test".into(), start_job: Box::new(|_| {}), askpass: PathBuf::new() };
        let mut output = Vec::new();
        serve_files(&input[..], &mut output, opts).unwrap();

        let (mut r, mut buf, mut replies) = (&output[..], Vec::new(), Vec::new());
        while let Some(Kind::Msg) = proto::read_frame(&mut r, &mut buf).unwrap() {
            replies.push(proto::parse_json::<Response>(&buf).unwrap());
        }
        match &replies[..] {
            [Response::Hello(_), Response::Places(p)] => {
                assert_eq!(p[0].name, "Home");
                assert_eq!(p[0].path, fsview::home().display().to_string());
            }
            other => panic!("unexpected replies: {other:?}"),
        }
    }
}
