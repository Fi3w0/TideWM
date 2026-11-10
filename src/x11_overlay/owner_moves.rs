//! Follow owner-authored X11 positioning without feeding satellite's own
//! ConfigureWindow requests back into the compositor. RECORD is optional;
//! it observes only ConfigureWindow requests, never keyboard or pointer data.

use std::{
    collections::HashSet,
    io,
    sync::{Arc, Mutex},
};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        record::{self, ConnectionExt as _},
        xproto,
    },
    rust_connection::RustConnection,
};

use super::{protocol_error, write_record};

#[derive(Debug, PartialEq, Eq)]
struct OwnerMove {
    id: u32,
    x: Option<i32>,
    y: Option<i32>,
}

/// Runs entirely in the observer child. The root-event connection owns the
/// context; closing that connection releases it. One child-only reader thread
/// consumes RECORD's reply stream on its required second connection.
pub(super) fn start(control: &RustConnection, tracked: Arc<Mutex<HashSet<u32>>>) -> io::Result<()> {
    if control
        .extension_information(record::X11_EXTENSION_NAME)
        .map_err(protocol_error)?
        .is_none()
    {
        eprintln!("X11 overlay observer: RECORD unavailable; client moves after mapping cannot be followed");
        return Ok(());
    }
    let (data, _) = x11rb::connect(None).map_err(protocol_error)?;
    let context = control.generate_id().map_err(protocol_error)?;
    let range = record::Range {
        core_requests: record::Range8 {
            first: xproto::CONFIGURE_WINDOW_REQUEST,
            last: xproto::CONFIGURE_WINDOW_REQUEST,
        },
        ..Default::default()
    };
    control
        .record_create_context(context, 0, &[record::CS::ALL_CLIENTS.into()], &[range])
        .map_err(protocol_error)?
        .check()
        .map_err(protocol_error)?;
    let resource_mask = control.setup().resource_id_mask;
    std::thread::Builder::new()
        .name("x11-owner-moves".into())
        .spawn(move || {
            if let Err(err) = run(data, context, resource_mask, tracked) {
                eprintln!("X11 overlay observer: owner-position stream stopped: {err}");
            }
        })?;
    Ok(())
}

fn run(
    data: RustConnection,
    context: record::Context,
    resource_mask: u32,
    tracked: Arc<Mutex<HashSet<u32>>>,
) -> io::Result<()> {
    let stream = data
        .record_enable_context(context)
        .map_err(protocol_error)?;
    data.flush().map_err(protocol_error)?;
    let mut stdout = io::stdout();
    for reply in stream {
        let reply = reply.map_err(protocol_error)?;
        // RECORD category FromClient. Other categories contain no requests.
        if reply.category != 1 {
            continue;
        }
        for movement in decode(
            &reply.data,
            reply.client_swapped,
            reply.xid_base,
            resource_mask,
        ) {
            if !tracked.lock().unwrap().contains(&movement.id) {
                continue;
            }
            write_record(
                &mut stdout,
                &serde_json::json!({
                    "kind":"move", "id":movement.id, "x":movement.x, "y":movement.y,
                }),
            )?;
        }
    }
    Ok(())
}

fn decode(data: &[u8], swapped: bool, owner: u32, resource_mask: u32) -> Vec<OwnerMove> {
    let u16_at = |bytes: &[u8]| {
        let value = u16::from_ne_bytes(bytes.try_into().unwrap());
        if swapped {
            value.swap_bytes()
        } else {
            value
        }
    };
    let u32_at = |bytes: &[u8]| {
        let value = u32::from_ne_bytes(bytes.try_into().unwrap());
        if swapped {
            value.swap_bytes()
        } else {
            value
        }
    };
    let mut moves = Vec::new();
    let mut remaining = data;
    while remaining.len() >= 4 {
        let length = usize::from(u16_at(&remaining[2..4])) * 4;
        if length < 12 || length > remaining.len() {
            break;
        }
        let (request, rest) = remaining.split_at(length);
        remaining = rest;
        if request[0] != xproto::CONFIGURE_WINDOW_REQUEST {
            continue;
        }
        let id = u32_at(&request[4..8]);
        let mask = u16_at(&request[8..10]);
        // Resource IDs identify the owning X11 connection using the live
        // server mask. Satellite moves foreign windows, so its requests fail
        // this check. No PID, application name or output name is involved.
        if id & !resource_mask != owner
            || mask & !0x7f != 0
            || length != 12 + mask.count_ones() as usize * 4
        {
            continue;
        }
        let mut values = request[12..].chunks_exact(4);
        let x = (mask & u16::from(xproto::ConfigWindow::X) != 0)
            .then(|| u32_at(values.next().unwrap()) as i32);
        let y = (mask & u16::from(xproto::ConfigWindow::Y) != 0)
            .then(|| u32_at(values.next().unwrap()) as i32);
        if x.is_some() || y.is_some() {
            moves.push(OwnerMove { id, x, y });
        }
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u32, mask: u16, values: &[i32], swapped: bool) -> Vec<u8> {
        let word16 = |v: u16| if swapped { v.swap_bytes() } else { v }.to_ne_bytes();
        let word32 = |v: u32| if swapped { v.swap_bytes() } else { v }.to_ne_bytes();
        let mut data = vec![xproto::CONFIGURE_WINDOW_REQUEST, 0];
        data.extend(word16((3 + values.len()) as u16));
        data.extend(word32(id));
        data.extend(word16(mask));
        data.extend([0, 0]);
        for value in values {
            data.extend(word32(*value as u32));
        }
        data
    }

    #[test]
    fn owner_moves_are_followed_but_satellite_feedback_is_not() {
        let data = request(0x400007, 3, &[1000, 650], false);
        assert_eq!(
            decode(&data, false, 0x400000, 0x1fffff),
            vec![OwnerMove {
                id: 0x400007,
                x: Some(1000),
                y: Some(650),
            }]
        );
        assert!(decode(&data, false, 0x200000, 0x1fffff).is_empty());
    }

    #[test]
    fn partial_coordinates_signed_values_and_swapped_clients() {
        for swapped in [false, true] {
            let data = request(0x400007, 2, &[-200], swapped);
            assert_eq!(
                decode(&data, swapped, 0x400000, 0x1fffff),
                vec![OwnerMove {
                    id: 0x400007,
                    x: None,
                    y: Some(-200),
                }]
            );
        }
    }

    #[test]
    fn resize_only_and_truncated_requests_do_not_move_a_window() {
        assert!(decode(
            &request(0x400007, 4, &[300], false),
            false,
            0x400000,
            0x1fffff
        )
        .is_empty());
        let mut data = request(0x400007, 3, &[1000, 650], false);
        data.pop();
        assert!(decode(&data, false, 0x400000, 0x1fffff).is_empty());
    }
}
