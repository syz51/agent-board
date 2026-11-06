use board_proto::{DeviceName, DeviceToken, Timestamp};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::auth::{hash_token, new_token};
use crate::db::{Ctx, parse_id, ts};
use crate::error::{Result, invalid, not_found};

/// A device as listed by `GET /v1/admin/devices` (never with its token).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: DeviceName,
    /// The token acts as the user.
    pub user: bool,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

/// Creates a device and returns its token, shown once. A revoked name may be reused.
pub(crate) fn create(cx: &mut Ctx<'_>, name: &DeviceName, user: bool) -> Result<DeviceToken> {
    let revoked: Option<Option<i64>> = cx
        .db
        .query_row(
            "SELECT revoked_at FROM devices WHERE name = ?1",
            [name.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(None) = revoked {
        return Err(invalid(
            format!("device {name} already exists"),
            format!("revoke it first (DELETE /v1/admin/devices/{name}) or pick another name"),
        ));
    }
    let token = new_token();
    cx.db.execute(
        "INSERT INTO devices (name, token_hash, user, created_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (name) DO UPDATE SET token_hash = ?2, user = ?3, created_at = ?4, \
         revoked_at = NULL",
        params![name.as_str(), hash_token(&token), user, cx.now_ms()],
    )?;
    Ok(DeviceToken {
        name: name.clone(),
        token,
    })
}

pub(crate) fn list(cx: &Ctx<'_>) -> Result<Vec<DeviceInfo>> {
    Ok(cx
        .db
        .prepare_cached("SELECT name, user, created_at, revoked_at FROM devices ORDER BY name")?
        .query_map([], |r| {
            Ok(DeviceInfo {
                name: parse_id(&r.get::<_, String>(0)?)?,
                user: r.get(1)?,
                created_at: ts(r.get(2)?),
                revoked_at: r.get::<_, Option<i64>>(3)?.map(ts),
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn revoke(cx: &mut Ctx<'_>, name: &DeviceName) -> Result<()> {
    let updated = cx.db.execute(
        "UPDATE devices SET revoked_at = ?2 WHERE name = ?1 AND revoked_at IS NULL",
        params![name.as_str(), cx.now_ms()],
    )?;
    if updated == 0 {
        return Err(not_found(
            format!("no active device named {name}"),
            "GET /v1/admin/devices lists the devices",
        ));
    }
    Ok(())
}
