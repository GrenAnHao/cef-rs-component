//! A bounded composition-character anchor, not an invented Chromium caret API.
//!
//! CEF reports a character rectangle list and an opaque document range. The
//! local IME selection is UTF-16-relative to the accepted composition. Do not
//! unconditionally use the last rectangle, infer selection from range names,
//! scale view DIP twice, or replace missing geometry with a surface-origin lie.
use rust_cef_core::{
    contracts::BrowserId,
    ime::{ImeSnapshot, ImeStatus, MAX_IME_CHARACTER_BOUNDS},
    ports::ImeGeometry,
};

pub fn composition_anchor(
    snapshot: &ImeSnapshot,
    browser: BrowserId,
    epoch: u64,
    selection: std::ops::Range<usize>,
    composition_utf16: usize,
    surface: (f32, f32, f32, f32),
) -> Option<ImeGeometry> {
    if snapshot.browser != browser
        || snapshot.revision == 0
        || snapshot.observed_input_epoch != epoch
        || snapshot.status != ImeStatus::Ready
        || composition_utf16 == 0
        || composition_utf16 > MAX_IME_CHARACTER_BOUNDS
        || snapshot.character_bounds.len() != composition_utf16
        || selection.start > selection.end
        || selection.end > composition_utf16
    {
        return None;
    }
    let range = snapshot.range?;
    if range.start > range.end
        || range.end - range.start != composition_utf16 as u32
        || ![surface.0, surface.1, surface.2, surface.3]
            .into_iter()
            .all(f32::is_finite)
        || surface.2 <= 0.0
        || surface.3 <= 0.0
    {
        return None;
    }
    // At an interior insertion point use the following character's leading
    // rectangle. At the composition end use the last trailing edge. This is a
    // horizontal composition anchor; RTL/vertical caret semantics need real
    // renderer evidence, not extrapolation from these rectangles.
    let at_end = selection.end == composition_utf16;
    let index = if at_end {
        composition_utf16 - 1
    } else {
        selection.end
    };
    let rect = snapshot.character_bounds.get(index)?;
    let right = rect.x.checked_add(rect.width)?;
    let bottom = rect.y.checked_add(rect.height)?;
    if rect.x < 0
        || rect.y < 0
        || rect.width < 0
        || rect.height <= 0
        || f64::from(right) > f64::from(surface.2)
        || f64::from(bottom) > f64::from(surface.3)
    {
        return None;
    }
    // A visible character is not proof that the entire synthesized caret
    // fits. In particular, its trailing edge may already be at the clip.
    let width = 1.0;
    let height = rect.height as f32;
    let local_x = if at_end { right } else { rect.x };
    Some(ImeGeometry {
        x: project_contained_axis(surface.0, local_x, width, surface.2)?,
        y: project_contained_axis(surface.1, rect.y, height, surface.3)?,
        width,
        height,
    })
}

fn project_contained_axis(origin: f32, offset: i32, length: f32, extent: f32) -> Option<f32> {
    let (origin, offset, length, extent) = (
        f64::from(origin),
        f64::from(offset),
        f64::from(length),
        f64::from(extent),
    );
    if offset + length > extent {
        return None;
    }
    let projected = (origin + offset) as f32;
    // Verify the value actually sent to the client-DIP consumer. Translating
    // and narrowing to f32 can round a fitting local point past the edge.
    let translated = f64::from(projected) - origin;
    // Do not substitute the surface origin if narrowing lost a nonzero offset.
    (projected.is_finite()
        && translated >= 0.0
        && (offset == 0.0 || translated > 0.0)
        && translated + length <= extent)
        .then_some(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{contracts::RuntimeGeneration, ime::ImeRect, input::Utf16Range};
    fn observation() -> ImeSnapshot {
        ImeSnapshot {
            browser: BrowserId::new(RuntimeGeneration::new(7), 2),
            revision: 8,
            observed_input_epoch: 3,
            range: Some(Utf16Range {
                start: 900,
                end: 903,
            }),
            character_bounds: (0..3)
                .map(|i| ImeRect {
                    x: 10 + i * 10,
                    y: 20,
                    width: 10,
                    height: 18,
                })
                .collect(),
            status: ImeStatus::Ready,
        }
    }
    #[test]
    fn local_utf16_cursor_selects_corresponding_rect_and_adds_origin_once() {
        let s = observation();
        for (cursor, x) in [(0, 60.0), (1, 70.0), (3, 90.0)] {
            assert_eq!(
                composition_anchor(
                    &s,
                    s.browser,
                    3,
                    cursor..cursor,
                    3,
                    (50.0, 60.0, 320.0, 240.0)
                ),
                Some(ImeGeometry {
                    x,
                    y: 80.0,
                    width: 1.0,
                    height: 18.0
                })
            );
        }
    }
    #[test]
    fn trailing_caret_requires_its_own_visible_width() {
        let s = observation();
        // Last CEF rectangle ends at x=40. It fits at width=40, but a
        // trailing 1-DIP caret does not. Never clamp to a different anchor.
        for width in [40.0, 40.5] {
            assert!(
                composition_anchor(&s, s.browser, 3, 3..3, 3, (50.25, 60.5, width, 38.0)).is_none()
            );
        }
        assert_eq!(
            composition_anchor(&s, s.browser, 3, 3..3, 3, (50.25, 60.5, 41.0, 38.0)),
            Some(ImeGeometry {
                x: 90.25,
                y: 80.5,
                width: 1.0,
                height: 18.0
            })
        );
        assert!(composition_anchor(&s, s.browser, 3, 2..2, 3, (50.25, 60.5, 40.0, 38.0)).is_some());
    }

    #[test]
    fn translated_f32_caret_cannot_round_past_the_visible_surface() {
        let mut s = observation();
        s.character_bounds[0] = ImeRect {
            x: 3,
            y: 3,
            width: 1,
            height: 1,
        };
        assert!(
            composition_anchor(&s, s.browser, 3, 0..0, 3, (16_777_216.0, 0.0, 4.0, 4.0)).is_none()
        );
        assert!(
            composition_anchor(&s, s.browser, 3, 0..0, 3, (0.0, 16_777_216.0, 4.0, 4.0)).is_none()
        );
        assert!(composition_anchor(&s, s.browser, 3, 0..0, 3, (0.0, 0.0, 4.0, 4.0)).is_some());
    }

    #[test]
    fn translated_caret_cannot_turn_nonzero_offset_into_surface_origin() {
        let mut s = observation();
        s.character_bounds[0] = ImeRect {
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        };
        for origin in [(16_777_216.0, 0.0), (0.0, 16_777_216.0)] {
            assert!(
                composition_anchor(&s, s.browser, 3, 0..0, 3, (origin.0, origin.1, 4.0, 4.0))
                    .is_none()
            );
        }
        s.character_bounds[0].x = 0;
        s.character_bounds[0].y = 0;
        assert_eq!(
            composition_anchor(
                &s,
                s.browser,
                3,
                0..0,
                3,
                (16_777_216.0, 16_777_216.0, 4.0, 4.0)
            ),
            Some(ImeGeometry {
                x: 16_777_216.0,
                y: 16_777_216.0,
                width: 1.0,
                height: 1.0
            })
        );
    }

    #[test]
    fn original_browser_and_observed_epoch_are_both_required() {
        let s = observation();
        assert!(composition_anchor(&s, s.browser, 4, 1..1, 3, (0.0, 0.0, 320.0, 240.0)).is_none());
        let other = BrowserId::new(RuntimeGeneration::new(8), 2);
        assert!(composition_anchor(&s, other, 3, 1..1, 3, (0.0, 0.0, 320.0, 240.0)).is_none());
        let mut unobserved = s.clone();
        unobserved.revision = 0;
        assert!(
            composition_anchor(&unobserved, s.browser, 3, 1..1, 3, (0.0, 0.0, 320.0, 240.0))
                .is_none()
        );
    }
    #[test]
    fn missing_or_unaligned_geometry_never_becomes_fallback_caret() {
        let s = observation();
        assert!(composition_anchor(&s, s.browser, 3, 1..1, 2, (0.0, 0.0, 320.0, 240.0)).is_none());
        assert!(composition_anchor(&s, s.browser, 3, 4..4, 3, (0.0, 0.0, 320.0, 240.0)).is_none());
        assert!(composition_anchor(&s, s.browser, 3, 1..1, 3, (0.0, 0.0, 15.0, 240.0)).is_none());
        let mut empty = s.clone();
        empty.character_bounds.clear();
        assert!(
            composition_anchor(&empty, empty.browser, 3, 0..0, 3, (0.0, 0.0, 320.0, 240.0))
                .is_none()
        );
    }
}
