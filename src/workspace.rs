pub struct Panels {
    pub header: f32,
    pub target: f32,
    pub summary: f32,
    pub status: f32,
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
        let compact = height < 650.0;
        let header = if compact { 50.0 } else { 60.0 };
        let target = if compact { 100.0 } else { 116.0 };
        let summary = if compact { 66.0 } else { 100.0 };
        let status = if compact { 28.0 } else { 34.0 };
        let left = if width >= 1180.0 { 250.0 } else { 185.0 };
        let right = if width >= 1180.0 {
            300.0
        } else if details_open {
            250.0
        } else {
            0.0
        };
        let available = height - header - target - summary - status;
        let map = if collapsed {
            34.0
        } else {
            (if height < 650.0 { 96.0 } else { map_height })
                .clamp(96.0, (available - 120.0).min(height * 0.4).max(96.0))
        };
        Self {
            header,
            target,
            summary,
            status,
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
    fn frame_regions_fit_without_hiding_the_list_at_supported_window_sizes() {
        for (width, height) in [
            (760.0, 500.0),
            (1024.0, 768.0),
            (1380.0, 880.0),
            (1600.0, 1040.0),
        ] {
            for details in [false, true] {
                let panels = Panels::new(width, height, 245.0, details, false);
                let occupied = panels.header
                    + panels.target
                    + panels.summary
                    + panels.body
                    + panels.map
                    + panels.status;
                assert!((occupied - height).abs() < 0.1);
                assert!(panels.center >= 300.0);
                assert!(panels.body >= 120.0);
                assert!(panels.target >= 100.0);
                assert!(panels.map >= 96.0);
            }
        }
    }
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
