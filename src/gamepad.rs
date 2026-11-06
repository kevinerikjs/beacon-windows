//! Controller passthrough: the phone's controller reports (phoros/docs/input.md) replayed into
//! a virtual Xbox 360 pad through the ViGEmBus driver, which every Windows game reads via XInput.

use std::sync::{Arc, OnceLock};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

static CLIENT: OnceLock<Option<Arc<Client>>> = OnceLock::new();

fn client() -> Option<Arc<Client>> {
    CLIENT.get_or_init(|| Client::connect().ok().map(Arc::new)).clone()
}

/// Whether the ViGEmBus driver is installed and answering. Advertised as
/// `supportsControllerInput`, so a phone only forwards a controller when this is true.
pub fn available() -> bool {
    client().is_some()
}

/// One phone's virtual pad. It plugs in on the first connected report and unplugs on a
/// report with the connected flag clear, or when dropped.
#[derive(Default)]
pub struct Pad {
    target: Option<Xbox360Wired<Arc<Client>>>,
    last_sequence: Option<u16>,
}

impl Pad {
    /// `payload` is a ControllerReport: 14 bytes, optionally a 2-byte sequence after them.
    pub fn handle(&mut self, payload: &[u8], connected: bool) {
        let Some(report) = parse(payload) else { return };
        // A client sending on TCP and UDP at once numbers its reports: the first copy wins.
        if let (Some(seq), Some(last)) = (report.1, self.last_sequence) {
            if (seq.wrapping_sub(last) as i16) <= 0 {
                return;
            }
        }
        if report.1.is_some() {
            self.last_sequence = report.1;
        }
        if !connected {
            if self.target.take().is_some() {
                log!("Virtual controller unplugged");
            }
            return;
        }
        if self.target.is_none() {
            let Some(client) = client() else { return };
            let mut pad = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            if pad.plugin().and_then(|_| pad.wait_ready()).is_err() {
                return log!("Could not plug in the virtual controller (ViGEmBus)");
            }
            log!("Virtual Xbox 360 controller plugged in");
            self.target = Some(pad);
        }
        if let Some(pad) = self.target.as_mut() {
            let _ = pad.update(&report.0);
        }
    }
}

/// Phoros button bits, bit 0 upward, and the XInput button each becomes.
const BUTTONS: [u16; 15] = [
    XButtons::A, XButtons::B, XButtons::X, XButtons::Y, XButtons::LB, XButtons::RB,
    XButtons::LTHUMB, XButtons::RTHUMB, XButtons::UP, XButtons::DOWN, XButtons::LEFT, XButtons::RIGHT,
    XButtons::START, XButtons::BACK, XButtons::GUIDE, // menu, options, home
];

fn parse(p: &[u8]) -> Option<(XGamepad, Option<u16>)> {
    if p.len() < 14 {
        return None;
    }
    let bits = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
    let raw = BUTTONS.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).fold(0u16, |a, (_, b)| a | b);
    let axis = |i: usize| i16::from_be_bytes([p[i], p[i + 1]]);
    // Both sides use right and up as positive, so sticks and triggers pass through unchanged.
    let pad = XGamepad {
        buttons: XButtons(raw),
        left_trigger: p[12],
        right_trigger: p[13],
        thumb_lx: axis(4),
        thumb_ly: axis(6),
        thumb_rx: axis(8),
        thumb_ry: axis(10),
    };
    let sequence = (p.len() >= 16).then(|| u16::from_be_bytes([p[14], p[15]]));
    Some((pad, sequence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_buttons_axes_and_sequence() {
        let mut p = vec![0u8; 16];
        p[..4].copy_from_slice(&(1u32 | 1 << 8 | 1 << 12 | 1 << 14).to_be_bytes()); // A, up, menu, home
        p[4..6].copy_from_slice(&(-32768i16).to_be_bytes());
        p[6..8].copy_from_slice(&32767i16.to_be_bytes());
        p[12] = 255;
        p[14..16].copy_from_slice(&7u16.to_be_bytes());
        let (pad, seq) = parse(&p).unwrap();
        assert_eq!(pad.buttons.raw, XButtons::A | XButtons::UP | XButtons::START | XButtons::GUIDE);
        assert_eq!((pad.thumb_lx, pad.thumb_ly, pad.left_trigger, pad.right_trigger), (-32768, 32767, 255, 0));
        assert_eq!(seq, Some(7));
        assert_eq!(parse(&p[..14]).unwrap().1, None);
        assert!(parse(&p[..13]).is_none());
    }
}
