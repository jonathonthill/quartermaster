//! The server side of the protocol: `archive-helper serve` runs this over
//! the SSH channel's stdin/stdout, answering requests against a local [`Store`].

use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::api::Archive;
use crate::catalog::{self as cat, NodeKind};
use crate::error::{Error, Result};
use crate::hash::Digest;
use crate::proto::{self, ErrorKind, HelloInfo, InitOptions, Kind, Request, Response, Trailer};
use crate::store::{Config, Store};
use crate::worker;

pub struct ServeOptions {
    pub root: PathBuf,
    /// Server-to-server keys: may add and read data, never delete or rename.
    pub restricted: bool,
    pub helper_version: String,
    /// Called when a job finishes or a client asks for maintenance
    /// (the helper starts a detached background worker).
    pub kick_worker: Option<Box<dyn FnMut() + Send>>,
}

/// Upload bytes arriving as `Data` frames, ending with `End{digest}` or `Abort`.
struct WirePayload<'a, R: Read> {
    r: &'a mut R,
    buf: Vec<u8>,
    pos: usize,
    finished: bool,
    aborted: Option<String>,
    digest: Option<Digest>,
}

impl<'a, R: Read> WirePayload<'a, R> {
    fn new(r: &'a mut R) -> Self {
        WirePayload { r, buf: Vec::new(), pos: 0, finished: false, aborted: None, digest: None }
    }

    /// Read one frame of the stream; false once it has ended.
    fn next(&mut self) -> io::Result<bool> {
        if self.finished {
            return Ok(false);
        }
        match proto::read_frame(self.r, &mut self.buf)? {
            Some(Kind::Data) => {
                self.pos = 0;
                Ok(true)
            }
            Some(Kind::End) => {
                let t: Trailer = proto::parse_json(&self.buf)?;
                self.digest = t.digest;
                self.finished = true;
                self.buf.clear();
                Ok(false)
            }
            Some(Kind::Abort) => {
                self.aborted = Some(String::from_utf8_lossy(&self.buf).into_owned());
                self.finished = true;
                self.buf.clear();
                Err(io::Error::other(format!("sender stopped: {}", self.aborted.as_deref().unwrap_or(""))))
            }
            Some(Kind::Msg) | None => {
                self.finished = true;
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "upload stream ended early"))
            }
        }
    }

    /// Consume the rest of the stream so the next request lines up.
    fn drain(&mut self) -> io::Result<()> {
        while !self.finished {
            match self.next() {
                Ok(_) => {}
                Err(_) if self.finished => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for WirePayload<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let k = out.len().min(self.buf.len() - self.pos);
                out[..k].copy_from_slice(&self.buf[self.pos..self.pos + k]);
                self.pos += k;
                return Ok(k);
            }
            if !self.next()? {
                return Ok(0);
            }
        }
    }
}

impl<R: Read> crate::api::Payload for WirePayload<'_, R> {
    fn digest(&self) -> Option<Digest> {
        self.digest
    }
}

fn respond(w: &mut impl Write, resp: &Response) -> io::Result<()> {
    proto::write_json(w, Kind::Msg, resp)?;
    w.flush()
}

fn denied(what: &str) -> Response {
    Response::Error { kind: ErrorKind::Denied, message: what.to_string() }
}

/// Serve requests until the client closes the connection.
pub fn serve(input: impl Read, output: impl Write, mut opts: ServeOptions) -> Result<()> {
    let mut r = BufReader::with_capacity(1 << 20, input);
    let mut w = BufWriter::with_capacity(1 << 20, output);
    let mut buf = Vec::new();
    let has_archive = opts.root.join("archive.json").exists();
    let mut store: Option<Store> = if has_archive { Some(Store::open(&opts.root)?) } else { None };
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
            respond(&mut w, &denied("say hello first"))?;
            continue;
        }
        if opts.restricted && !req.allowed_when_restricted() {
            respond(&mut w, &denied("this connection can only add and read data"))?;
            continue;
        }
        if let Request::Hello { version, .. } = &req {
            if *version != proto::PROTOCOL_VERSION {
                respond(
                    &mut w,
                    &Response::Error {
                        kind: ErrorKind::Other,
                        message: format!("helper speaks protocol {}, client {version}; update the helper", proto::PROTOCOL_VERSION),
                    },
                )?;
                return Ok(());
            }
            greeted = true;
            let info = HelloInfo::here(
                opts.root.display().to_string(),
                store.as_ref().map(|s| s.config().archive_id.clone()),
                opts.restricted,
                opts.helper_version.clone(),
            );
            respond(&mut w, &Response::Hello(info))?;
            continue;
        }
        if let Request::Init { options } = &req {
            let resp = if store.is_some() {
                Response::from_error(&Error::AlreadyExists(format!("an archive already exists at {}", opts.root.display())))
            } else {
                match init(&opts, options) {
                    Ok(s) => {
                        store = Some(s);
                        Response::Ok
                    }
                    Err(e) => Response::from_error(&e),
                }
            };
            respond(&mut w, &resp)?;
            continue;
        }
        let Some(st) = store.as_mut() else {
            respond(&mut w, &Response::from_error(&Error::NotFound(format!("no archive at {}", opts.root.display()))))?;
            continue;
        };

        let resp = match req {
            Request::PutFile { job, req } => match st.precheck_put(&req.dest, req.policy, &job) {
                Err(e) => Response::from_error(&e),
                Ok(Some(out)) => Response::Outcome(out),
                Ok(None) => {
                    respond(&mut w, &Response::Proceed)?;
                    let mut payload = WirePayload::new(&mut r);
                    let res = st.put_file(&job, &req, &mut payload);
                    payload.drain()?;
                    match res {
                        Ok(out) => Response::Outcome(out),
                        Err(e) => Response::from_error(&e),
                    }
                }
            },
            Request::PutSolid { job, req } => {
                let pre: Result<Vec<_>> = req.members.iter().map(|m| st.precheck_put(&m.dest, req.policy, &job)).collect();
                match pre {
                    Err(e) => Response::from_error(&e),
                    Ok(pre) if pre.iter().all(|p| p.is_some()) => Response::Outcomes(pre.into_iter().flatten().collect()),
                    Ok(_) => {
                        respond(&mut w, &Response::Proceed)?;
                        let mut payload = WirePayload::new(&mut r);
                        let res = st.put_solid(&job, &req, &mut payload);
                        payload.drain()?;
                        match res {
                            Ok(outs) => Response::Outcomes(outs),
                            Err(e) => Response::from_error(&e),
                        }
                    }
                }
            }
            Request::ReadFile { path } => {
                send_file(st, &path, &mut w)?;
                w.flush()?;
                continue;
            }
            Request::ReadMany { paths } => {
                for p in &paths {
                    send_file(st, p, &mut w)?;
                }
                w.flush()?;
                continue;
            }
            Request::Settle => match st.settle() {
                Ok(f) => Response::Failed(f),
                Err(e) => Response::from_error(&e),
            },
            Request::FinishJob { job } => match st.finish_job(&job) {
                Ok(()) => {
                    if let Some(k) = opts.kick_worker.as_mut() {
                        k();
                    }
                    Response::Ok
                }
                Err(e) => Response::from_error(&e),
            },
            Request::KickWorker => {
                if let Some(k) = opts.kick_worker.as_mut() {
                    k();
                }
                Response::Ok
            }
            Request::WorkerStatus => match worker::read_status(&opts.root) {
                Some(s) => Response::Json(serde_json::to_value(s)?),
                None => Response::Json(serde_json::Value::Null),
            },
            Request::Packs => wrap(cat::packs(st.conn()).map(Response::Packs)),
            Request::Stat { path } => wrap(st.stat(&path).map(Response::Entry)),
            Request::List { path } => wrap(st.list(&path).map(Response::Entries)),
            Request::Walk { path } => wrap(st.walk(&path).map(Response::Walk)),
            Request::SizesPresent { scope, sizes } => wrap(st.sizes_present(&scope, &sizes).map(Response::Bools)),
            Request::Have { scope, hashes } => wrap(st.have(&scope, &hashes).map(Response::Bools)),
            Request::Mkdirs { job, dirs } => wrap(st.mkdirs(&job, &dirs).map(|_| Response::Ok)),
            Request::Symlink { job, dest, target, mtime_ns, policy } => {
                wrap(st.symlink(&job, &dest, &target, mtime_ns, policy).map(Response::Outcome))
            }
            Request::Link { job, req, sha256 } => wrap(st.link(&job, &req, &sha256).map(Response::Outcome)),
            Request::LinkMany { job, items } => {
                wrap(st.link_many(&job, &items).map(|rs| Response::Results(rs.into_iter().map(proto::ItemResult::from_result).collect())))
            }
            Request::Info { path } => wrap(Archive::info(st, &path).map(Response::Info)),
            Request::Search { query, limit } => wrap(Archive::search(st, &query, limit.min(1000)).map(Response::Search)),
            Request::CreateFolder { path } => wrap(st.create_folder(&path).map(|_| Response::Ok)),
            Request::Rename { from, to } => wrap(Archive::rename(st, &from, &to).map(|_| Response::Ok)),
            Request::Trash { path } => wrap(Archive::trash(st, &path).map(Response::Id)),
            Request::Restore { id, to } => wrap(Archive::restore(st, id, to.as_ref()).map(Response::Path)),
            Request::TrashList => wrap(Archive::trash_list(st).map(Response::Entries)),
            Request::KeyAuthorize { key, label, from } => wrap((|| {
                let helper = std::env::current_exe()?.display().to_string();
                crate::keys::authorize(&helper, &opts.root.display().to_string(), &key, &label, from.as_deref())?;
                Ok(Response::Ok)
            })()),
            Request::KeyList => {
                Response::Keys(crate::keys::list().into_iter().filter(|k| k.root == opts.root.display().to_string()).collect())
            }
            Request::KeyRevoke { label } => wrap(crate::keys::revoke(&label).map(|_| Response::Ok)),
            Request::Hello { .. } | Request::Init { .. } => unreachable!("handled above"),
            _ => Response::Error { kind: ErrorKind::Other, message: "that request is for file servers, not archives".into() },
        };
        respond(&mut w, &resp)?;
    }
}

fn wrap(r: Result<Response>) -> Response {
    r.unwrap_or_else(|e| Response::from_error(&e))
}

fn init(opts: &ServeOptions, o: &InitOptions) -> Result<Store> {
    let mut cfg = Config::default();
    if let Some(v) = o.pack_target_bytes {
        cfg.pack_target_bytes = v;
    }
    if let Some(v) = o.par2_redundancy_percent {
        cfg.par2_redundancy_percent = v;
    }
    if let Some(v) = o.trash_days {
        cfg.trash_days = v;
    }
    Store::init(&opts.root, cfg)
}

/// Answer one `ReadFile`: a `FileStart`, one `Data` per zstd frame, then `End`
/// (or a single error reply). Large files go out as their stored frames. A
/// small file inside a shared block is decompressed here and sent as its own
/// frame, so the transfer is proportional to the file, not the block.
fn send_file(st: &mut Store, path: &crate::vpath::VPath, w: &mut impl Write) -> Result<()> {
    let plan = (|| -> Result<_> {
        let e = cat::resolve(st.conn(), path)?.ok_or_else(|| Error::NotFound(path.to_string()))?;
        let cid = match (e.kind, e.content_id) {
            (NodeKind::File, Some(c)) => c,
            _ => return Err(Error::InvalidPath(format!("{path} is not a file"))),
        };
        let plan = st.frame_plan(cid)?;
        let stored: u64 = plan.frames.iter().map(|f| f.d_len as u64).sum();
        if stored > 2 * plan.len + (64 << 10) {
            let mut raw = Vec::with_capacity(plan.len as usize);
            st.open_content(cid)?.read_to_end(&mut raw)?;
            let frame = zstd::bulk::compress(&raw, 1)?;
            return Ok((e, None, vec![(frame, raw.len() as u32)]));
        }
        Ok((e, Some(plan), Vec::new()))
    })();
    let (entry, plan, own) = match plan {
        Ok(p) => p,
        Err(e) => return Ok(proto::write_json(w, Kind::Msg, &Response::from_error(&e))?),
    };
    match plan {
        None => {
            let frames = own.iter().map(|(_, d)| *d).collect();
            let len = entry.size;
            proto::write_json(w, Kind::Msg, &Response::FileStart { entry, frames, skip: 0, len })?;
            for (f, _) in &own {
                proto::write_frame(w, Kind::Data, f)?;
            }
        }
        Some(mut plan) => {
            let frames = plan.frames.iter().map(|f| f.d_len).collect();
            proto::write_json(w, Kind::Msg, &Response::FileStart { entry, frames, skip: plan.skip, len: plan.len })?;
            let mut cbuf = Vec::new();
            for fr in &plan.frames {
                cbuf.resize(fr.c_len as usize, 0);
                let read = plan.file.seek(SeekFrom::Start(plan.start + fr.c_off)).and_then(|_| plan.file.read_exact(&mut cbuf));
                if let Err(e) = read {
                    proto::write_frame(w, Kind::Abort, format!("reading the archive failed: {e}").as_bytes())?;
                    return Ok(());
                }
                proto::write_frame(w, Kind::Data, &cbuf)?;
            }
        }
    }
    proto::write_json(w, Kind::End, &Trailer::default())?;
    Ok(())
}
