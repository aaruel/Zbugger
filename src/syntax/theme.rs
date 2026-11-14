use egui::Color32;

use super::TokenKind;

/// Colors for each [`TokenKind`].
pub struct Theme {
    pub plain: Color32,
    pub keyword: Color32,
    pub ty: Color32,
    pub function: Color32,
    pub number: Color32,
    pub string: Color32,
    pub comment: Color32,
    pub preprocessor: Color32,
    pub punctuation: Color32,
    pub constant: Color32,
    pub namespace: Color32,
    pub property: Color32,
}

impl Theme {
    pub const DARK: Theme = Theme {
        plain: Color32::from_rgb(0xD4, 0xD4, 0xD4),
        keyword: Color32::from_rgb(0x56, 0x9C, 0xD6),
        ty: Color32::from_rgb(0x4E, 0xC9, 0xB0),
        function: Color32::from_rgb(0xDC, 0xDC, 0xAA),
        number: Color32::from_rgb(0xB5, 0xCE, 0xA8),
        string: Color32::from_rgb(0xCE, 0x91, 0x78),
        comment: Color32::from_rgb(0x6A, 0x99, 0x55),
        preprocessor: Color32::from_rgb(0xC5, 0x86, 0xC0),
        punctuation: Color32::from_rgb(0xB4, 0xB4, 0xB4),
        constant: Color32::from_rgb(0x4F, 0xC1, 0xFF),
        namespace: Color32::from_rgb(0x4E, 0xC9, 0xB0),
        property: Color32::from_rgb(0x9C, 0xDC, 0xFE),
    };

    pub const LIGHT: Theme = Theme {
        plain: Color32::from_rgb(0x1F, 0x1F, 0x1F),
        keyword: Color32::from_rgb(0x00, 0x00, 0xFF),
        ty: Color32::from_rgb(0x26, 0x7F, 0x99),
        function: Color32::from_rgb(0x79, 0x5E, 0x26),
        number: Color32::from_rgb(0x09, 0x86, 0x58),
        string: Color32::from_rgb(0xA3, 0x15, 0x15),
        comment: Color32::from_rgb(0x00, 0x80, 0x00),
        preprocessor: Color32::from_rgb(0xAF, 0x00, 0xDB),
        punctuation: Color32::from_rgb(0x40, 0x40, 0x40),
        constant: Color32::from_rgb(0x00, 0x70, 0xC1),
        namespace: Color32::from_rgb(0x26, 0x7F, 0x99),
        property: Color32::from_rgb(0x00, 0x10, 0x80),
    };

    pub fn for_visuals(visuals: &egui::Visuals) -> &'static Theme {
        if visuals.dark_mode {
            &Self::DARK
        } else {
            &Self::LIGHT
        }
    }

    pub fn color(&self, kind: TokenKind) -> Color32 {
        match kind {
            TokenKind::Plain => self.plain,
            TokenKind::Keyword => self.keyword,
            TokenKind::Type => self.ty,
            TokenKind::Function => self.function,
            TokenKind::Number => self.number,
            TokenKind::String => self.string,
            TokenKind::Comment => self.comment,
            TokenKind::Preprocessor => self.preprocessor,
            TokenKind::Punctuation => self.punctuation,
            TokenKind::Constant => self.constant,
            TokenKind::Namespace => self.namespace,
            TokenKind::Property => self.property,
        }
    }
}
