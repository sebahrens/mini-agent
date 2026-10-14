//! Allocation-free, bounded preflight for values about to enter JSON merging.
//! JSON serializes non-finite floats as null; rejecting them preserves policy.
use serde::Serialize;
use serde::ser::{
    self, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
};

#[derive(Debug)]
struct Error;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("config normalization refuses non-finite values or excessive structure")
    }
}
impl std::error::Error for Error {}
impl ser::Error for Error {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Self
    }
}

pub(super) fn validate<T: Serialize>(value: &T) -> Result<(), &'static str> {
    value
        .serialize(&mut Check { depth: 0, items: 0 })
        .map_err(|_| "config normalization refuses non-finite values or excessive structure")
}
struct Check {
    depth: usize,
    items: usize,
}
impl Check {
    fn scalar(&mut self) -> Result<(), Error> {
        self.items += 1;
        if self.items > 65_536 {
            Err(Error)
        } else {
            Ok(())
        }
    }
    fn compound(&mut self) -> Result<Compound<'_>, Error> {
        self.scalar()?;
        if self.depth >= 128 {
            return Err(Error);
        }
        self.depth += 1;
        Ok(Compound(self))
    }
    fn wrapped<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        let compound = self.compound()?;
        value.serialize(&mut *compound.0)
    }
}
struct Compound<'a>(&'a mut Check);
impl Drop for Compound<'_> {
    fn drop(&mut self) {
        self.0.depth -= 1;
    }
}
impl<'a> ser::Serializer for &'a mut Check {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Compound<'a>;
    type SerializeTuple = Compound<'a>;
    type SerializeTupleStruct = Compound<'a>;
    type SerializeTupleVariant = Compound<'a>;
    type SerializeMap = Compound<'a>;
    type SerializeStruct = Compound<'a>;
    type SerializeStructVariant = Compound<'a>;

    fn serialize_bool(self, _: bool) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_i8(self, _: i8) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_i16(self, _: i16) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_i32(self, _: i32) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_i64(self, _: i64) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_i128(self, _: i128) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_u8(self, _: u8) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_u16(self, _: u16) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_u32(self, _: u32) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_u64(self, _: u64) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_u128(self, _: u128) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_f32(self, value: f32) -> Result<(), Error> {
        if !value.is_finite() {
            return Err(Error);
        }
        self.scalar()
    }
    fn serialize_f64(self, value: f64) -> Result<(), Error> {
        if !value.is_finite() {
            return Err(Error);
        }
        self.scalar()
    }
    fn serialize_char(self, _: char) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_str(self, _: &str) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_none(self) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Error> {
        self.wrapped(value)
    }
    fn serialize_unit(self) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<(), Error> {
        self.scalar()
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.wrapped(value)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.wrapped(value)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn serialize_tuple(self, _: usize) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Compound<'a>, Error> {
        self.compound()
    }
    fn collect_str<T: ?Sized + std::fmt::Display>(self, _: &T) -> Result<(), Error> {
        self.scalar()
    }
}
impl SerializeSeq for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
impl SerializeTuple for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
impl SerializeTupleStruct for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
impl SerializeTupleVariant for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
impl SerializeMap for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.0)
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
impl SerializeStruct for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.0.scalar()?;
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
impl SerializeStructVariant for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.0.scalar()?;
        value.serialize(&mut *self.0)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Serialize)]
    enum Shape {
        Unit,
        Tuple(f32, u64),
        Record { hidden: f64 },
    }
    #[test]
    fn config69_finite_preflight_checks_nested_values_without_names_or_rewrites() {
        validate(&(
            Shape::Unit,
            Some(Shape::Tuple(1.5, u64::MAX)),
            Shape::Record { hidden: 0.0 },
        ))
        .unwrap();
        validate(&serde_json::json!({"null":null,"unsigned":u64::MAX})).unwrap();
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(validate(&Some(Shape::Tuple(value, 1))).is_err());
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                validate(&std::collections::BTreeMap::from([(
                    "PRIVATE_NAME",
                    vec![Shape::Record { hidden: value }]
                )]))
                .is_err()
            );
        }
    }
    struct Nested(usize);
    impl Serialize for Nested {
        fn serialize<S: ser::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            if self.0 == 0 {
                s.serialize_unit()
            } else {
                s.serialize_newtype_struct("nested", &Nested(self.0 - 1))
            }
        }
    }
    #[test]
    fn config69_finite_preflight_bounds_wrappers_and_items() {
        validate(&Nested(127)).unwrap();
        assert!(validate(&Nested(129)).is_err());
        assert!(validate(&vec![0u8; 65_537]).is_err());
        validate(&vec![0u8; 10]).unwrap();
    }
}
