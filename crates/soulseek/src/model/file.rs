//! Shared file and directory models.

/// Type of a file attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileAttributeType {
    BitRate,
    Duration,
    VariableBitRate,
    Encoder,
    SampleRate,
    BitDepth,
    /// A value not known to this implementation, preserved as-is.
    Unknown(u32),
}

impl From<u32> for FileAttributeType {
    fn from(v: u32) -> Self {
        match v {
            0 => Self::BitRate,
            1 => Self::Duration,
            2 => Self::VariableBitRate,
            3 => Self::Encoder,
            4 => Self::SampleRate,
            5 => Self::BitDepth,
            other => Self::Unknown(other),
        }
    }
}

impl From<FileAttributeType> for u32 {
    fn from(t: FileAttributeType) -> u32 {
        match t {
            FileAttributeType::BitRate => 0,
            FileAttributeType::Duration => 1,
            FileAttributeType::VariableBitRate => 2,
            FileAttributeType::Encoder => 3,
            FileAttributeType::SampleRate => 4,
            FileAttributeType::BitDepth => 5,
            FileAttributeType::Unknown(v) => v,
        }
    }
}

/// A file attribute (type + value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileAttribute {
    pub kind: FileAttributeType,
    pub value: u32,
}

impl FileAttribute {
    pub fn new(kind: FileAttributeType, value: u32) -> Self {
        Self { kind, value }
    }
}

/// A shared file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct File {
    pub code: u8,
    pub filename: String,
    pub size: u64,
    pub extension: String,
    pub attributes: Vec<FileAttribute>,
}

impl File {
    fn attribute(&self, kind: FileAttributeType) -> Option<u32> {
        self.attributes
            .iter()
            .find(|a| a.kind == kind)
            .map(|a| a.value)
    }

    pub fn attribute_count(&self) -> usize {
        self.attributes.len()
    }

    /// Bit rate in kbps.
    pub fn bit_rate(&self) -> Option<u32> {
        self.attribute(FileAttributeType::BitRate)
    }

    /// Length (duration) in seconds.
    pub fn length(&self) -> Option<u32> {
        self.attribute(FileAttributeType::Duration)
    }

    pub fn is_variable_bit_rate(&self) -> Option<bool> {
        self.attribute(FileAttributeType::VariableBitRate)
            .map(|v| v != 0)
    }

    pub fn sample_rate(&self) -> Option<u32> {
        self.attribute(FileAttributeType::SampleRate)
    }

    pub fn bit_depth(&self) -> Option<u32> {
        self.attribute(FileAttributeType::BitDepth)
    }
}

/// A directory with its files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Directory {
    pub name: String,
    pub files: Vec<File>,
}

impl Directory {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_helpers() {
        let f = File {
            attributes: vec![
                FileAttribute::new(FileAttributeType::BitRate, 320),
                FileAttribute::new(FileAttributeType::Duration, 200),
                FileAttribute::new(FileAttributeType::VariableBitRate, 1),
                FileAttribute::new(FileAttributeType::SampleRate, 44100),
                FileAttribute::new(FileAttributeType::BitDepth, 16),
            ],
            ..Default::default()
        };
        assert_eq!(f.bit_rate(), Some(320));
        assert_eq!(f.length(), Some(200));
        assert_eq!(f.is_variable_bit_rate(), Some(true));
        assert_eq!(f.sample_rate(), Some(44100));
        assert_eq!(f.bit_depth(), Some(16));
        assert_eq!(File::default().bit_rate(), None);
    }

    #[test]
    fn attribute_type_conversion() {
        for v in 0..8u32 {
            assert_eq!(u32::from(FileAttributeType::from(v)), v);
        }
        assert_eq!(FileAttributeType::from(9), FileAttributeType::Unknown(9));
    }
}
