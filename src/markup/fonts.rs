use std::sync::Arc;

use cosmic_text::fontdb::Source as CosmicTextFontSource;
use fontdb::Database;
#[cfg(test)]
use fontdb::{Family, Query};

pub const PROSE_FONT_FAMILY: &str = "Mona Sans";
pub const CODE_FONT_FAMILY: &str = "Monaspace Neon";

const MONA_SANS: &[u8] = include_bytes!("../../assets/fonts/MonaSans.ttf");
const MONASPACE_NEON: &[u8] = include_bytes!("../../assets/fonts/MonaspaceNeon.otf");

pub fn bundled_font_sources() -> Vec<CosmicTextFontSource> {
    vec![
        CosmicTextFontSource::Binary(Arc::new(MONA_SANS.to_vec())),
        CosmicTextFontSource::Binary(Arc::new(MONASPACE_NEON.to_vec())),
    ]
}

pub fn load_bundled_fonts(database: &mut Database) {
    database.load_font_data(MONA_SANS.to_vec());
    database.load_font_data(MONASPACE_NEON.to_vec());
}

#[cfg(test)]
fn has_family(database: &Database, family: &str) -> bool {
    database
        .query(&Query {
            families: &[Family::Name(family)],
            ..Query::default()
        })
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_expose_expected_families() {
        let mut database = Database::new();
        load_bundled_fonts(&mut database);

        assert!(has_family(&database, PROSE_FONT_FAMILY));
        assert!(has_family(&database, CODE_FONT_FAMILY));
    }
}
