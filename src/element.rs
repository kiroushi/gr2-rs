//! Typed element tree for extracted GR2 struct data.
//!
//! `Field` pairs a member name + type with an extracted `Value`.
//! References store raw flat-buffer offsets (lazy, not eagerly resolved).

use crate::format::MemberType;

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub member_type: MemberType,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int8(i8),
    UInt8(u8),
    Int16(i16),
    UInt16(u16),
    Int32(i32),
    UInt32(u32),
    Real16(u16),
    Real32(f32),
    String(Option<String>),
    Struct(Vec<Field>),
    Reference {
        offset: Option<usize>,
    },
    ReferenceToArray {
        count: u32,
        offset: Option<usize>,
    },
    ArrayOfReferences {
        count: u32,
        offset: Option<usize>,
    },
    VariantReference {
        type_offset: Option<usize>,
        data_offset: Option<usize>,
    },
    ReferenceToVariantArray {
        type_offset: Option<usize>,
        count: u32,
        data_offset: Option<usize>,
    },
    Transform {
        flags: u32,
        translation: [f32; 3],
        rotation: [f32; 4],
        scale_shear: [[f32; 3]; 3],
    },
    EmptyReference,
    Array(Vec<Value>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_clone_equality() {
        let a = Value::UInt32(42);
        let b = a.clone();
        assert_eq!(a, b);

        let s = Value::String(Some("hello".into()));
        assert_eq!(s, s.clone());
    }

    #[test]
    fn value_nan_inequality() {
        let a = Value::Real32(f32::NAN);
        let b = Value::Real32(f32::NAN);
        // NaN != NaN by IEEE 754, and PartialEq for f32 respects this
        assert_ne!(a, b);
    }

    #[test]
    fn value_array_nested() {
        let arr = Value::Array(vec![
            Value::Struct(vec![Field {
                name: "x".into(),
                member_type: MemberType::Real32,
                value: Value::Real32(1.0),
            }]),
            Value::Struct(vec![Field {
                name: "x".into(),
                member_type: MemberType::Real32,
                value: Value::Real32(2.0),
            }]),
        ]);
        if let Value::Array(items) = &arr {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn value_transform_fields() {
        let t = Value::Transform {
            flags: 7,
            translation: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale_shear: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        if let Value::Transform { flags, .. } = &t {
            assert_eq!(*flags, 7);
        } else {
            panic!("expected Transform");
        }
    }

    #[test]
    fn field_construction() {
        let f = Field {
            name: "Position".into(),
            member_type: MemberType::Real32,
            value: Value::Array(vec![Value::Real32(1.0), Value::Real32(2.0), Value::Real32(3.0)]),
        };
        assert_eq!(f.name, "Position");
        assert_eq!(f.member_type, MemberType::Real32);
        if let Value::Array(vals) = &f.value {
            assert_eq!(vals.len(), 3);
        } else {
            panic!("expected Array");
        }
    }
}
