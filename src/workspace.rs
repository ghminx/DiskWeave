pub struct Panels {
    pub left: f32,
    pub center: f32,
    pub right: f32,
    pub map: f32,
    pub body: f32,
}
impl Panels {
    pub fn new(
        width: f32,
        height: f32,
        map_height: f32,
        details_open: bool,
        collapsed: bool,
    ) -> Self {
        let left = if width >= 1180.0 { 230.0 } else { 188.0 };
        let right = if width >= 1180.0 {
            280.0
        } else if details_open {
            250.0
        } else {
            0.0
        };
        let available = (height - 240.0).max(154.0);
        let map = if collapsed {
            34.0
        } else {
            (if height < 650.0 { 96.0 } else { map_height })
                .clamp(96.0, (available - 120.0).min(height * 0.4).max(96.0))
        };
        Self {
            left,
            center: width - left - right,
            right,
            map,
            body: available - map,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_and_large_windows_reserve_visible_workspace_and_bounded_map() {
        let small = Panels::new(760.0, 500.0, 900.0, false, false);
        assert_eq!(small.right, 0.0);
        assert!(small.center >= 300.0);
        assert!(small.body >= 120.0);
        assert!(small.map >= 96.0 && small.map <= 140.0);
        let detail = Panels::new(760.0, 500.0, 200.0, true, false);
        assert!(detail.right > 0.0 && detail.center >= 300.0);
        let wide = Panels::new(1600.0, 1000.0, 200.0, false, false);
        assert!(wide.right >= 260.0 && wide.center >= 800.0);
        assert_eq!(wide.map, 200.0);
        assert!((wide.left + wide.center + wide.right - 1600.0).abs() < 0.01);
        let collapsed = Panels::new(760.0, 500.0, 200.0, false, true);
        assert_eq!(collapsed.map, 34.0);
        assert!(collapsed.body > small.body);
    }
}
