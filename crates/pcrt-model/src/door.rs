//! Общие типы телеметрии дверей.

/// Идентификатор физической двери, поддерживаемый действующим протоколом.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DoorId(u8);

impl DoorId {
    /// Создаёт идентификатор двери от 1 до 4.
    #[must_use]
    pub const fn new(value: u8) -> Option<Self> {
        if value >= 1 && value <= 4 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Возвращает числовое представление идентификатора.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Состояние двери в RS-232 протоколе.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoorState {
    Closed,
    Open,
}

impl DoorState {
    /// Преобразует совместимый с текущим протоколом байт `0` или `1`.
    #[must_use]
    pub const fn from_protocol_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Closed),
            1 => Some(Self::Open),
            _ => None,
        }
    }

    /// Кодирует состояние в байт действующего протокола.
    #[must_use]
    pub const fn protocol_byte(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::Open => 1,
        }
    }
}

/// Значения, полученные с контроллера двери.
///
/// `voltage_raw` сохраняет 16-битное big-endian значение как есть. Единица
/// измерения не определена, пока не подтверждена документацией устройства.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DoorTelemetry {
    pub state: DoorState,
    pub voltage_raw: u16,
}

#[cfg(test)]
mod tests {
    use super::{DoorId, DoorState};

    #[test]
    fn accepts_currently_supported_door_range() {
        assert_eq!(DoorId::new(1).map(DoorId::get), Some(1));
        assert_eq!(DoorId::new(4).map(DoorId::get), Some(4));
        assert_eq!(DoorId::new(0), None);
        assert_eq!(DoorId::new(5), None);
    }

    #[test]
    fn protocol_state_is_lossless() {
        assert_eq!(DoorState::from_protocol_byte(0), Some(DoorState::Closed));
        assert_eq!(DoorState::from_protocol_byte(1), Some(DoorState::Open));
        assert_eq!(DoorState::from_protocol_byte(2), None);
        assert_eq!(DoorState::Open.protocol_byte(), 1);
    }
}
