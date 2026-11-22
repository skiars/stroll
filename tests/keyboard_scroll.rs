#[path = "../vendor/gpui-pre-0.3.12/src/scroll_navigation.rs"]
mod scroll_navigation;

use std::time::Duration;

use scroll_navigation::NavigationAxis;

const FRAME: Duration = Duration::from_millis(8);
const KEY_STEPS: [(&str, f32); 4] = [
    ("PageUp", 200.),
    ("PageDown", -200.),
    ("Up", 20.),
    ("Down", -20.),
];

#[test]
fn directional_key_repeats_carry_the_current_speed() {
    for (key, step) in KEY_STEPS {
        for frames_per_repeat in [4, 8] {
            let mut position = 0.;
            let mut target = step;
            let mut motion = NavigationAxis::new(position, target);
            for _ in 0..8 {
                for _ in 0..frames_per_repeat {
                    let next = motion.advance(FRAME);
                    assert!((next - position) * step > 0., "{key}");
                    assert!((target - next) * step >= 0., "{key}");
                    position = next;
                }

                target += step;
                let velocity = motion.velocity();
                assert!(velocity * step > 0., "{key}");
                motion.retarget(position, target);
                assert_eq!(motion.advance(Duration::ZERO), position, "{key}");
                assert!((motion.velocity() - velocity).abs() < 0.01, "{key}");
            }
        }
    }
}

#[test]
fn repeating_against_the_boundary_keeps_the_same_arrival_as_releasing() {
    for (key, step) in KEY_STEPS {
        for frames_per_repeat in [4, 8] {
            let edge = step * 5.5;
            let mut position = 0.;
            let mut target = step;
            let mut motion = NavigationAxis::new(position, target);
            for _ in 0..5 {
                for _ in 0..frames_per_repeat {
                    position = motion.advance(FRAME);
                }
                target = (target + step).clamp(edge.min(0.), edge.max(0.));
                motion.retarget(position, target);
            }
            assert_eq!(target, edge);
            assert_ne!(position, edge);
            let mut released = motion;
            for frame in 1..=24 {
                let next = motion.advance(FRAME);
                assert!((edge - next) * step >= 0., "{key}");
                assert!((next - position) * step >= 0., "{key}");
                assert_eq!(next, released.advance(FRAME), "{key}");
                if frame % frames_per_repeat == 0 {
                    motion.retarget(next, edge);
                }
                assert_eq!(motion.velocity(), released.velocity(), "{key}");
                assert_eq!(motion.is_finished(), released.is_finished(), "{key}");
                position = next;
            }
            assert!(motion.is_finished(), "{key}");
            assert_eq!(position, edge, "{key}");
            assert_eq!(motion.velocity(), 0., "{key}");
        }
    }
}

#[test]
fn single_key_press_keeps_the_140_ms_ease_out() {
    for (key, target) in KEY_STEPS {
        let mut motion = NavigationAxis::new(0., target);
        for elapsed_ms in 1..=140 {
            let actual = motion.advance(Duration::from_millis(1));
            let remaining = 1. - elapsed_ms as f32 / 140.;
            let expected = target - target * remaining.powi(3);
            assert!(
                (actual - expected).abs() < 0.001,
                "{key} at {elapsed_ms} ms"
            );
        }
        assert!(motion.is_finished(), "{key}");
        assert_eq!(motion.velocity(), 0., "{key}");
    }
}

#[test]
fn retargeting_to_a_closer_offset_brakes_without_overshooting() {
    let mut motion = NavigationAxis::new(0., 200.);
    let current = motion.advance(FRAME);
    let velocity = motion.velocity();
    let target = current + 2.;
    motion.retarget(current, target);
    assert!((motion.velocity() - velocity).abs() < 0.01);
    let next = motion.advance(Duration::from_millis(1));
    assert!((current..=target).contains(&next));
    assert_eq!(motion.advance(FRAME), target);
    assert!(motion.is_finished());
}
