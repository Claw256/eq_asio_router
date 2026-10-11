//! "Fun" section: JBL Bass Mode (car-subwoofer bass boost, distortion and pumping).

use crate::app::App;

pub fn ui(ui: &mut egui::Ui, app: &mut App) {
    egui::CollapsingHeader::new(egui::RichText::new("Fun").heading()).default_open(true).show(ui, |ui| {
        let mut changed = false;
        changed |= ui
            .checkbox(&mut app.cfg.jbl_bass_mode, "JBL Bass Mode")
            .on_hover_text(
                "Bass boost, clipping distortion and bass-keyed pumping on top of the active AutoEQ preset, \
                 like an overdriven car subwoofer.",
            )
            .changed();
        let mut pct = i32::from(app.cfg.jbl_intensity);
        let on = app.cfg.jbl_bass_mode;
        if ui
            .add_enabled(on, egui::Slider::new(&mut pct, 0..=100).suffix("%").text("Intensity"))
            .changed()
        {
            app.cfg.jbl_intensity = pct.clamp(0, 100) as u8;
            changed = true;
        }
        if changed {
            app.on_edited(false);
        }
    });
}
