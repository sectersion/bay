//! Wire protocol between the bay CLI and bayd (#14). One JSON object per
//! line per direction on a socket connection.
//!
//! Responses are internally tagged — a reply is exactly one of:
//! - `{"type":"ok", ...}` — success; payload fields are added as commands land
//! - `{"type":"error","error":"<TOKEN>"}` — failure; `error` is a machine token
//!
//! The `type` tag makes a reply carrying both ok and error fields impossible
//! to produce and impossible to parse (#22).
//!
//! Tolerance policy (deliberate, do not "tighten"): receivers MUST ignore
//! unknown fields — `deny_unknown_fields` is intentionally absent. Senders
//! may add fields to `ok` payloads without a version bump; old clients keep
//! parsing such replies as success.
//!
//! Requests are also internally tagged, but with tag key `cmd` (`{"cmd":"ping"}`).
//! The duality is deliberate: `cmd` for intent, `type` for outcome.
//!
//! Contract for later M1 workers: `Response::Ok` gains payload fields as
//! commands land; `dispatch` is where stateless commands become real.
//! Commands that need the connection or fleet state (Up, Exec) are routed
//! in `daemon::handle` before reaching `dispatch`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Hello,
    Ping,
    Bake {
        manifest: PathBuf,
    },
    Up {
        n: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<PathBuf>,
    },
    Exec {
        bay: u32,
        // `cmd` is taken by the tag; the command to run is `argv` on the wire.
        argv: Vec<String>,
    },
    Halt {
        bay: u32,
    },
    Wake {
        bay: u32,
    },
    Kill {
        #[serde(skip_serializing_if = "Option::is_none")]
        bay: Option<u32>,
        all: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Response {
    Ok,
    #[serde(rename = "error")]
    Err { error: String },
}

pub const NOT_IMPLEMENTED: &str = "NOT_IMPLEMENTED";

pub fn dispatch(req: &Request) -> Response {
    match req {
        Request::Ping => Response::Ok,
        _ => Response::Err {
            error: NOT_IMPLEMENTED.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cases() -> Vec<Request> {
        vec![
            Request::Hello,
            Request::Ping,
            Request::Bake {
                manifest: "/x/golden.toml".into(),
            },
            Request::Up { n: 2, repo: None },
            Request::Up {
                n: 1,
                repo: Some("/r".into()),
            },
            Request::Exec {
                bay: 1,
                argv: vec!["echo".into(), "hi".into()],
            },
            Request::Halt { bay: 1 },
            Request::Wake { bay: 1 },
            Request::Kill { bay: None, all: true },
            Request::Kill {
                bay: Some(3),
                all: false,
            },
        ]
    }

    #[test]
    fn request_wire_shape() {
        assert_eq!(
            serde_json::to_string(&Request::Ping).unwrap(),
            r#"{"cmd":"ping"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"up","n":2}"#).unwrap(),
            Request::Up { n: 2, repo: None }
        );
    }

    #[test]
    fn request_round_trip() {
        for req in cases() {
            let s = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), req, "{s}");
        }
    }

    #[test]
    fn response_wire_shape() {
        assert_eq!(
            serde_json::to_string(&Response::Ok).unwrap(),
            r#"{"type":"ok"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::Err {
                error: "NOT_IMPLEMENTED".to_string()
            })
            .unwrap(),
            r#"{"type":"error","error":"NOT_IMPLEMENTED"}"#
        );
        assert_eq!(
            serde_json::from_str::<Response>(r#"{"type":"ok"}"#).unwrap(),
            Response::Ok
        );
        assert_eq!(
            serde_json::from_str::<Response>(r#"{"type":"error","error":"NOT_IMPLEMENTED"}"#)
                .unwrap(),
            Response::Err {
                error: "NOT_IMPLEMENTED".to_string()
            }
        );
    }

    #[test]
    fn reply_cannot_be_both_ok_and_error() {
        // regression for #22: under the old untagged enum, a reply carrying
        // both fields parsed as Ok and the error vanished silently.
        assert!(serde_json::from_str::<Response>(r#"{"ok":true,"error":"boom"}"#).is_err());
        // no tag at all (old error shape) is no longer valid
        assert!(serde_json::from_str::<Response>(r#"{"error":"boom"}"#).is_err());
        // the error arm cannot be faked as ok: `error` field is required
        assert!(serde_json::from_str::<Response>(r#"{"type":"error"}"#).is_err());
    }

    #[test]
    fn dispatch_ping_is_ok() {
        assert_eq!(dispatch(&Request::Ping), Response::Ok);
    }

    #[test]
    fn dispatch_stubs_are_not_implemented() {
        for req in cases() {
            if req == Request::Ping {
                continue;
            }
            assert_eq!(
                dispatch(&req),
                Response::Err {
                    error: NOT_IMPLEMENTED.to_string()
                },
                "{req:?}"
            );
        }
    }
}
