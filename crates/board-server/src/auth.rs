//! Who is calling: a device (by bearer token) or the user (by token or Tailscale identity),
//! plus the agent the device speaks for (`X-Board-Agent`).

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use board_proto::{AGENT_HEADER, AgentId, DeviceName};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::db::parse_id;
use crate::error::{Error, Result, forbidden, invalid, unauthorized};
use crate::state::AppState;

/// Header `tailscale serve` sets to the logged-in user's login name.
pub(crate) const IDENTITY_HEADER: &str = "tailscale-user-login";

#[derive(Debug, Clone)]
pub(crate) enum Role {
    /// A device token; `user` tokens act as the user too.
    Device { name: DeviceName, user: bool },
    /// The browser user, identified by `tailscale serve`.
    User,
}

/// An authenticated caller.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    pub role: Role,
    /// The agent named in `X-Board-Agent`, if any.
    pub agent: Option<AgentId>,
}

impl Caller {
    pub fn is_user(&self) -> bool {
        matches!(self.role, Role::User | Role::Device { user: true, .. })
    }

    pub fn require_user(&self) -> Result<()> {
        if self.is_user() {
            Ok(())
        } else {
            Err(forbidden(
                "only the user may do this",
                "ask the user (ask_user), or use a token created with --user",
            ))
        }
    }

    /// Checks that the request speaks for `agent`: `X-Board-Agent` must name it.
    pub fn act_as(&self, agent: &AgentId) -> Result<()> {
        match &self.agent {
            Some(header) if header == agent => Ok(()),
            Some(header) => Err(forbidden(
                format!("X-Board-Agent is {header} but the request acts for {agent}"),
                "send the same agent id in X-Board-Agent and in the body",
            )),
            None => Err(invalid(
                format!("X-Board-Agent is missing; the request acts for {agent}"),
                format!("send the header X-Board-Agent: {agent}"),
            )),
        }
    }

    /// Checks that a device token acts for its own device; the user may name any device.
    pub fn act_for_device(&self, device: &DeviceName) -> Result<()> {
        match self.device() {
            Some(own) if own != device && !self.is_user() => Err(forbidden(
                format!("this token belongs to device {own}, not {device}"),
                format!("send your own device: device={own}"),
            )),
            _ => Ok(()),
        }
    }

    /// The agent from `X-Board-Agent`, required.
    pub fn agent(&self) -> Result<&AgentId> {
        self.agent.as_ref().ok_or_else(|| {
            invalid(
                "X-Board-Agent is missing",
                "send the header X-Board-Agent: <your agent id>",
            )
        })
    }

    /// The event actor: the agent, else `user`.
    pub fn actor(&self) -> String {
        match &self.agent {
            Some(agent) => agent.to_string(),
            None => "user".to_owned(),
        }
    }

    /// Where a user answer came from.
    pub fn answered_via(&self) -> &'static str {
        match self.role {
            Role::User => "web",
            Role::Device { .. } => "cli",
        }
    }

    pub fn device(&self) -> Option<&DeviceName> {
        match &self.role {
            Role::Device { name, .. } => Some(name),
            Role::User => None,
        }
    }
}

/// Lowercase hex SHA-256 of a token, as stored.
pub(crate) fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// A new random 32-byte token, hex-encoded.
pub(crate) fn new_token() -> String {
    random_hex::<32>()
}

/// `N` random bytes, hex-encoded.
pub(crate) fn random_hex<const N: usize>() -> String {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    hex(&bytes)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    headers
        .get(name)
        .map(|v| {
            v.to_str().map_err(|_| {
                invalid(
                    format!("header {name} is not ASCII text"),
                    format!("send {name} as plain text"),
                )
            })
        })
        .transpose()
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let agent = header(&parts.headers, AGENT_HEADER)?
            .map(|v| {
                v.parse::<AgentId>().map_err(|e| {
                    invalid(
                        format!("X-Board-Agent: {e}"),
                        "send your agent id, such as t3:<thread id> or claude:<session id>",
                    )
                })
            })
            .transpose()?;
        let bearer = header(&parts.headers, "authorization")?
            .map(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
                    .map(str::trim)
                    .ok_or_else(|| {
                        unauthorized(
                            "Authorization must be a bearer token",
                            "send Authorization: Bearer <device token>",
                        )
                    })
            })
            .transpose()?;
        if let Some(token) = bearer {
            let hash = hash_token(token);
            let device = state
                .read(move |cx| {
                    Ok(cx
                        .db
                        .query_row(
                            "SELECT name, user FROM devices WHERE token_hash = ?1 \
                             AND revoked_at IS NULL",
                            [&hash],
                            |r| {
                                Ok((
                                    parse_id::<DeviceName>(&r.get::<_, String>(0)?)?,
                                    r.get::<_, bool>(1)?,
                                ))
                            },
                        )
                        .optional()?)
                })
                .await?;
            let Some((name, user)) = device else {
                return Err(unauthorized(
                    "the device token is unknown or revoked",
                    "log in again with `board login` and a new token from the user",
                ));
            };
            return Ok(Caller {
                role: Role::Device { name, user },
                agent,
            });
        }
        if state.trusts_identity_header()
            && let Some(login) = header(&parts.headers, IDENTITY_HEADER)?
        {
            if state.is_user_login(login) {
                return Ok(Caller {
                    role: Role::User,
                    agent,
                });
            }
            return Err(forbidden(
                format!("Tailscale login {login:?} is not one of the server's users"),
                "add the login to `users` in the server config",
            ));
        }
        Err(unauthorized(
            "no credentials",
            "send Authorization: Bearer <device token> (run `board login`)",
        ))
    }
}
