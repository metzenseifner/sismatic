//! The inventory routes — what devices and groups this server was configured
//! with.
//!
//! Every path below is relative to the `/v1/inventory` scope
//! [`crate::startup`] mounts them under.
//!
//! ```text
//! GET /devices             every configured device
//! GET /devices/{id}        one device, with the latest value of each field
//! GET /groups              every configured group
//! GET /groups/{id}         one group and the devices it addresses
//! ```
//!
//! # Why these are not answered from the store
//!
//! Every other read route answers from what the sync side *wrote*, which is why
//! an unknown device there yields an empty list rather than a `404` — the store
//! cannot tell "no such device" from "this one has not answered yet". These
//! four answer from the [`DeviceCatalog`] instead, which is the configured set,
//! so here a `404` is a real claim: the id is not in the devices file.
//!
//! That distinction is what makes the index useful. `GET /v1/inventory/devices` returning
//! `[]` from the store would mean "nothing has been polled yet"; returning `[]`
//! from the catalog means "no devices are configured", and those call for very
//! different actions.
//!
//! # `status` is live, and only here
//!
//! The catalog's [`DeviceSummary::status`] is always `unknown` — it is a
//! snapshot of configuration taken before the process connected to anything.
//! Both device routes overlay the real value from [`DeviceStatus`], which reads
//! the running registry without dialing. That makes these the only two routes
//! whose answer can change with nothing having been written.
//!
//! [`DeviceSummary::status`]: sismatic_api_types::DeviceSummary::status
//!
//! # The one route that reads both
//!
//! [`read_device`] joins the two sides: the catalog says the device exists and
//! how it is addressed, the store says what it last reported. A device that is
//! configured but has never answered is a `200` with an empty `latest`, which
//! is the honest answer and the one a dashboard needs to render a row at all.

use actix_web::web;
use sismatic_api_types::{ApiError, DeviceDetail, DeviceList, GroupList, GroupSummary};
use sismatic_store::ReadStore;
use sismatic_store::catalog::DeviceCatalog;
use sismatic_store::status::DeviceStatus;

use crate::handlers::error::ApiFailure;
use crate::handlers::target::{INVENTORY, reject_group_bare};

/// `GET /v1/inventory/devices` — every configured device, ordered by id.
#[utoipa::path(
    get,
    path = "/devices",
    context_path = "/v1/inventory",
    tag = "inventory",
    responses(
        (status = 200, description = "Every device in the devices file, ordered by \
             id, each with its live connection state. Empty means none are \
             configured — unlike an empty reads list, which means none have \
             answered.", body = DeviceList),
    ),
)]
pub async fn list_devices(
    catalog: web::Data<dyn DeviceCatalog>,
    status: web::Data<dyn DeviceStatus>,
) -> web::Json<DeviceList> {
    // The catalog is a snapshot of configuration taken before the process
    // connected to anything, so the `status` it carries is always `Unknown`.
    // The live value is overlaid here, which is the only place both are in
    // scope. One `all()` for the fleet rather than a lookup per device: the
    // adapter walks its registry once, and a per-device call would walk it once
    // each.
    let live = status.all().await;
    let devices = catalog
        .devices()
        .await
        .into_iter()
        .map(|mut device| {
            // A device in the catalog and absent from the registry stays
            // `Unknown` — the two are built from one config, so it cannot
            // happen, and inventing `Cold` for it would be a claim rather than
            // an observation.
            if let Some(&observed) = live.get(&device.id) {
                device.status = observed;
            }
            device
        })
        .collect();
    web::Json(DeviceList { devices })
}

/// `GET /v1/inventory/devices/{id}` — one device and the latest value of every
/// field it has reported.
#[utoipa::path(
    get,
    path = "/devices/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Device id, as written in the devices file.")),
    responses(
        (status = 200, description = "The device, with one read per field it has \
             answered. `latest` is empty for a device that is configured but has \
             never been reached.", body = DeviceDetail),
        (status = 404, description = "No device has this id, or the id names a device \
             group — in which case the body carries `/v1/inventory/groups/{id}`. Unlike the \
             reads routes' 404, this one is a claim about configuration.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_device(
    catalog: web::Data<dyn DeviceCatalog>,
    status: web::Data<dyn DeviceStatus>,
    store: web::Data<dyn ReadStore>,
    path: web::Path<String>,
) -> Result<web::Json<DeviceDetail>, ApiFailure> {
    let id = path.into_inner();
    // A group id was already a 404 here, since the lookup is device-only. What
    // this adds is the other half of the message: which URL answers instead.
    reject_group_bare(&**catalog, &id, INVENTORY).await?;
    let mut device = catalog
        .device(&id)
        .await
        .ok_or_else(|| ApiFailure::NotFound(format!("no device '{id}' is configured")))?;
    device.status = status.status(&id).await;

    // Only after the device is known to exist: a store read for an unknown id
    // would answer `[]` and turn a 404 into a plausible-looking 200.
    let latest = store.latest_all(id).await?;
    Ok(web::Json(DeviceDetail { device, latest }))
}

/// `GET /v1/inventory/groups` — every configured group, ordered by id.
#[utoipa::path(
    get,
    path = "/groups",
    context_path = "/v1/inventory",
    tag = "inventory",
    responses(
        (status = 200, description = "Every group in the devices file, ordered by id. \
             Each carries its members in the order they were configured.",
         body = GroupList),
    ),
)]
pub async fn list_groups(catalog: web::Data<dyn DeviceCatalog>) -> web::Json<GroupList> {
    web::Json(GroupList {
        groups: catalog.groups().await,
    })
}

/// `GET /v1/inventory/groups/{id}` — one group and the devices it addresses.
#[utoipa::path(
    get,
    path = "/groups/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Group id, as written in the devices file.")),
    responses(
        (status = 200, description = "The group and its member device ids, in \
             configured order.", body = GroupSummary),
        (status = 404, description = "No group has this id.", body = ApiError),
    ),
)]
pub async fn read_group(
    catalog: web::Data<dyn DeviceCatalog>,
    path: web::Path<String>,
) -> Result<web::Json<GroupSummary>, ApiFailure> {
    let id = path.into_inner();
    if let Some(group) = catalog.group(&id).await {
        return Ok(web::Json(group));
    }

    // Devices and groups share one id namespace, so naming a device here is a
    // different mistake from naming nothing: the fix is a different URL, not a
    // different id. Saying which saves the caller a second round trip to find
    // out.
    let message = if catalog.device(&id).await.is_some() {
        format!("'{id}' is a device, not a group; try /v1/inventory/devices/{id}")
    } else {
        format!("no group '{id}' is configured")
    };
    Err(ApiFailure::NotFound(message))
}
