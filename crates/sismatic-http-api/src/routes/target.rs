//! What an id in a path resolves to, and which URL space it belongs in.
//!
//! Devices and groups share one id namespace — the config layer guarantees they
//! never collide, so at most one lookup can match — and the API has two URL
//! spaces over it. That pairing needs exactly two rules, and they are here
//! rather than in the route modules so that every route enforces the same one:
//!
//! - [`group_members`]: this route is under `/v1/groups`, so the id must name a
//!   device group. A device id is refused.
//! - [`reject_group`]: this route is under `/v1/devices`, so the id must not
//!   name a device group. Everything else is the route's own business.
//!
//! # Why a refusal rather than a fan-out
//!
//! `/v1/devices/{id}` used to accept a group id and expand it across the
//! members, because that was the only way to address a device group before
//! `/v1/groups` existed. Keeping it would leave two URLs doing one write and —
//! worse — two routes that *cannot* answer correctly for a group at all:
//! `GET /v1/devices/{id}/recording` and `GET /v1/devices/{id}/commands` read an
//! outbox keyed by device, so a group id there takes a default and reports
//! `idle` at epoch `0` with an empty queue. That is a confident answer about a
//! device which does not exist, and no amount of documentation makes it safe.
//!
//! So the namespace is shared and the *spaces* are not. Each refusal names the
//! URL in the other space that answers the same question, so a caller that
//! reached for the wrong one is told where to go rather than only that it was
//! wrong.
//!
//! # What `reject_group` deliberately does not do
//!
//! It is not an existence check. An id that names nothing at all passes it, and
//! the route decides what to do — which is what preserves the readings routes'
//! answer for an unknown device: `[]`, not `404`, because the store cannot tell
//! "no such device" from "this one has not answered yet" and a `404` would
//! report an unreachable device as an unconfigured one (see
//! [`crate::routes::readings`]).
//!
//! Only a *positive* group hit is a claim, and it is one the catalog is
//! entitled to make: it holds the configured set, so "this id is a device
//! group" is a fact rather than an inference from absence.

use sismatic_api_types::DeviceId;
use sismatic_store::catalog::DeviceCatalog;

use crate::routes::error::ApiFailure;

/// The devices a group id addresses, or the `404` that says it addresses none.
///
/// [`DeviceCatalog::members`] would answer for a device id too, wrapping it in a
/// one-element list — right for the fan-out inside `submit`, which does not care
/// which kind it holds, and wrong on every `/v1/groups` route, where a device id
/// would silently produce a one-member "group" that is not a group. So the
/// lookup is [`DeviceCatalog::group`].
///
/// `device_route` is the tail of the `/v1/devices` route that answers the same
/// question.
pub(crate) async fn group_members(
    catalog: &dyn DeviceCatalog,
    id: &str,
    device_route: &str,
) -> Result<Vec<DeviceId>, ApiFailure> {
    if let Some(group) = catalog.group(id).await {
        return Ok(group.members);
    }

    let message = if catalog.device(id).await.is_some() {
        format!("'{id}' is a device, not a device group; try /v1/devices/{id}/{device_route}")
    } else {
        format!("no device group '{id}' is configured")
    };
    Err(ApiFailure::NotFound(message))
}

/// Refuse an id that names a device group, on a route under `/v1/devices`.
///
/// `group_route` is the tail of the `/v1/groups` route that answers the same
/// question, so the message is a redirection rather than a complaint.
///
/// Returns `Ok(())` for an id that names a device *and* for an id that names
/// nothing — see the module docs for why the second is not this function's
/// business.
pub(crate) async fn reject_group(
    catalog: &dyn DeviceCatalog,
    id: &str,
    group_route: &str,
) -> Result<(), ApiFailure> {
    if catalog.group(id).await.is_some() {
        return Err(ApiFailure::NotFound(format!(
            "'{id}' is a device group, not a device; try /v1/groups/{id}/{group_route}"
        )));
    }
    Ok(())
}

/// [`reject_group`] for the two routes whose `/v1/groups` counterpart has no
/// path tail: `GET /v1/devices/{id}` against `GET /v1/groups/{id}`.
pub(crate) async fn reject_group_bare(
    catalog: &dyn DeviceCatalog,
    id: &str,
) -> Result<(), ApiFailure> {
    if catalog.group(id).await.is_some() {
        return Err(ApiFailure::NotFound(format!(
            "'{id}' is a device group, not a device; try /v1/groups/{id}"
        )));
    }
    Ok(())
}
