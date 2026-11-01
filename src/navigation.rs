#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum Screen {
    #[default]
    Home = 0,
    Apks = 1,
    Private = 2,
    Settings = 3,
}

impl TryFrom<i32> for Screen {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Home),
            1 => Ok(Self::Apks),
            2 => Ok(Self::Private),
            3 => Ok(Self::Settings),
            _ => Err(()),
        }
    }
}

impl Screen {
    pub const fn index(self) -> i32 {
        self as i32
    }
}

#[cfg(test)]
mod tests {
    use super::Screen;

    #[test]
    fn screen_index_round_trips() {
        for index in 0..=3 {
            assert_eq!(Screen::try_from(index).unwrap().index(), index);
        }
    }

    #[test]
    fn invalid_screen_is_rejected() {
        assert!(Screen::try_from(4).is_err());
    }
}
