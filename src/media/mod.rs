pub mod cmaf;
pub mod container;
pub mod init;
pub mod segment;

/// Four-character code (FourCC) used to identify MP4 box types.
pub type FourCC = [u8; 4];

/// Well-known box type constants.
pub mod box_type {
    use super::FourCC;

    pub const FTYP: FourCC = *b"ftyp";
    pub const MOOV: FourCC = *b"moov";
    pub const MVHD: FourCC = *b"mvhd";
    pub const TRAK: FourCC = *b"trak";
    pub const TKHD: FourCC = *b"tkhd";
    pub const MDIA: FourCC = *b"mdia";
    pub const MDHD: FourCC = *b"mdhd";
    pub const HDLR: FourCC = *b"hdlr";
    pub const MINF: FourCC = *b"minf";
    pub const STBL: FourCC = *b"stbl";
    pub const STSD: FourCC = *b"stsd";
    pub const SINF: FourCC = *b"sinf";
    pub const FRMA: FourCC = *b"frma";
    pub const SCHM: FourCC = *b"schm";
    pub const SCHI: FourCC = *b"schi";
    pub const TENC: FourCC = *b"tenc";
    pub const PSSH: FourCC = *b"pssh";
    pub const MOOF: FourCC = *b"moof";
    pub const MFHD: FourCC = *b"mfhd";
    pub const TRAF: FourCC = *b"traf";
    pub const TFHD: FourCC = *b"tfhd";
    pub const TFDT: FourCC = *b"tfdt";
    pub const TRUN: FourCC = *b"trun";
    pub const SENC: FourCC = *b"senc";
    pub const SAIZ: FourCC = *b"saiz";
    pub const SAIO: FourCC = *b"saio";
    pub const MDAT: FourCC = *b"mdat";
    pub const SBGP: FourCC = *b"sbgp";
    pub const SGPD: FourCC = *b"sgpd";
    pub const MVEX: FourCC = *b"mvex";
    pub const TREX: FourCC = *b"trex";
    pub const EDTS: FourCC = *b"edts";
}

/// Track types as identified by the handler type in hdlr box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackType {
    Video,
    Audio,
    Subtitle,
    Unknown,
}

impl TrackType {
    pub fn from_handler(handler: &FourCC) -> Self {
        match handler {
            b"vide" => TrackType::Video,
            b"soun" => TrackType::Audio,
            b"subt" | b"text" => TrackType::Subtitle,
            _ => TrackType::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_is_4_bytes() {
        assert_eq!(std::mem::size_of::<FourCC>(), 4);
    }

    #[test]
    fn box_type_constants_are_correct() {
        assert_eq!(&box_type::FTYP, b"ftyp");
        assert_eq!(&box_type::MOOV, b"moov");
        assert_eq!(&box_type::MOOF, b"moof");
        assert_eq!(&box_type::MDAT, b"mdat");
        assert_eq!(&box_type::TRAK, b"trak");
        assert_eq!(&box_type::SINF, b"sinf");
        assert_eq!(&box_type::SCHM, b"schm");
        assert_eq!(&box_type::TENC, b"tenc");
        assert_eq!(&box_type::PSSH, b"pssh");
        assert_eq!(&box_type::SENC, b"senc");
        assert_eq!(&box_type::TRUN, b"trun");
    }

    #[test]
    fn track_type_video() {
        assert_eq!(TrackType::from_handler(b"vide"), TrackType::Video);
    }

    #[test]
    fn track_type_audio() {
        assert_eq!(TrackType::from_handler(b"soun"), TrackType::Audio);
    }

    #[test]
    fn track_type_subtitle() {
        assert_eq!(TrackType::from_handler(b"subt"), TrackType::Subtitle);
        assert_eq!(TrackType::from_handler(b"text"), TrackType::Subtitle);
    }

    #[test]
    fn track_type_unknown() {
        assert_eq!(TrackType::from_handler(b"hint"), TrackType::Unknown);
        assert_eq!(TrackType::from_handler(b"meta"), TrackType::Unknown);
    }
}
