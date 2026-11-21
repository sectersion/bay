//! Wire protocol between the bay CLI and bayd (#14). One JSON object per
//! line per direction on a socket connection.
//!
//! Contract for later M1 workers: `Response::Ok` gains payload fields as
//! commands land; `dispatch` is the single place stubs become real.

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
#[serde(untagged)]
pub enum Response {
    Ok { ok: bool },
    Err { error: String },
}

pub const NOT_IMPLEMENTED: &str = "NOT_IMPLEMENTED";

pub fn dispatch(req: &Request) -> Response {
    match req {
        Request::Ping => Response::Ok { ok: true },
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
            serde_json::to_string(&Response::Ok { ok: true }).unwrap(),
            r#"{"ok":true}"#
        );
        assert_eq!(
            serde_json::from_str::<Response>(r#"{"error":"NOT_IMPLEMENTED"}"#).unwrap(),
            Response::Err {
                error: "NOT_IMPLEMENTED".to_string()
            }
        );
    }

    #[test]
    fn dispatch_ping_is_ok() {
        assert_eq!(dispatch(&Request::Ping), Response::Ok { ok: true });
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
