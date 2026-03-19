use crate::Size;
use gerber_parser::gerber_types::{
    Aperture, ApertureDefinition, ApertureMacro, Command, CoordinateNumber, DCode, ExtendedCode,
    FunctionCode, MacroContent, MacroDecimal, Operation, StepAndRepeat, Unit,
};

/// A trait that allows conversion between mm and in
pub(crate) trait UnitAble
where
    Self: Sized,
{
    /// Converts known mm to given Unit
    fn mm_to_unit(&self, unit: &Unit) -> Self;

    /// Converts from given unit to mm
    fn to_mm(&self, unit: &Unit) -> Self;

    /// Converts from given unit to a given unit
    fn to_unit(&self, from: &Unit, to: &Unit) -> Self {
        self.to_mm(from).mm_to_unit(to)
    }

    /// Converts from given unit to a given unit and stores it into current struct
    fn convert_unit_self(&mut self, from: &Unit, to: &Unit) {
        *self = self.to_unit(from, to);
    }
}

impl UnitAble for f64 {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        match unit {
            Unit::Inches => self / 25.4,
            Unit::Millimeters => *self,
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        match unit {
            Unit::Inches => self * 25.4,
            Unit::Millimeters => *self,
        }
    }
}

impl UnitAble for CoordinateNumber {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        CoordinateNumber::try_from(f64::from(*self).mm_to_unit(unit)).unwrap()
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        CoordinateNumber::try_from(f64::from(*self).to_mm(unit)).unwrap()
    }
}

impl<T> UnitAble for Option<T>
where
    T: UnitAble + Sized + Copy,
{
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        self.map(|n| n.to_mm(unit))
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        self.map(|n| n.to_mm(unit))
    }
}

impl UnitAble for Size {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        Self {
            height: self.height.mm_to_unit(unit),
            width: self.width.mm_to_unit(unit),
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        Self {
            width: self.width.to_mm(unit),
            height: self.height.to_mm(unit),
        }
    }
}

impl UnitAble for MacroDecimal {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        match self {
            MacroDecimal::Value(v) => MacroDecimal::Value(v.mm_to_unit(unit)),
            dec => dec.clone(),
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        match self {
            MacroDecimal::Value(v) => MacroDecimal::Value(v.to_mm(unit)),
            dec => dec.clone(),
        }
    }
}

impl<A, B> UnitAble for (A, B)
where
    A: UnitAble + Sized,
    B: UnitAble + Sized,
{
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let (a, b) = self;
        (a.mm_to_unit(unit), b.mm_to_unit(unit))
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let (a, b) = self;
        (a.to_mm(unit), b.to_mm(unit))
    }
}

impl UnitAble for MacroContent {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            MacroContent::Circle(c) => {
                c.diameter = c.diameter.mm_to_unit(unit);
                c.center = c.center.mm_to_unit(unit);
            }
            MacroContent::VectorLine(v) => {
                v.width = v.width.mm_to_unit(unit);
                v.start = v.start.mm_to_unit(unit);
                v.end = v.end.mm_to_unit(unit);
            }
            MacroContent::CenterLine(l) => {
                l.center = l.center.mm_to_unit(unit);
                l.dimensions = l.dimensions.mm_to_unit(unit);
            }
            MacroContent::Outline(o) => {
                o.points.iter_mut().for_each(|x| *x = x.mm_to_unit(unit));
            }
            MacroContent::Polygon(p) => {
                p.center = p.center.mm_to_unit(unit);
                p.diameter = p.diameter.mm_to_unit(unit);
            }
            MacroContent::Moire(m) => {
                m.center = m.center.mm_to_unit(unit);
                m.diameter = m.diameter.mm_to_unit(unit);
                m.cross_hair_length = m.cross_hair_length.mm_to_unit(unit);
                m.cross_hair_thickness = m.cross_hair_thickness.mm_to_unit(unit);
                m.gap = m.gap.mm_to_unit(unit);
                m.ring_thickness = m.ring_thickness.mm_to_unit(unit);
            }
            MacroContent::Thermal(t) => {
                t.center = t.center.mm_to_unit(unit);
                t.gap = t.gap.mm_to_unit(unit);
                t.inner_diameter = t.inner_diameter.mm_to_unit(unit);
                t.outer_diameter.mm_to_unit(unit);
            }
            MacroContent::VariableDefinition(_) => {}
            MacroContent::Comment(_) => {}
        }
        cloned
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            MacroContent::Circle(c) => {
                c.diameter = c.diameter.to_mm(unit);
                c.center = c.center.to_mm(unit);
            }
            MacroContent::VectorLine(v) => {
                v.width = v.width.to_mm(unit);
                v.start = v.start.to_mm(unit);
                v.end = v.end.to_mm(unit);
            }
            MacroContent::CenterLine(l) => {
                l.center = l.center.to_mm(unit);
                l.dimensions = l.dimensions.to_mm(unit);
            }
            MacroContent::Outline(o) => {
                o.points.iter_mut().for_each(|x| *x = x.to_mm(unit));
            }
            MacroContent::Polygon(p) => {
                p.center = p.center.to_mm(unit);
                p.diameter = p.diameter.to_mm(unit);
            }
            MacroContent::Moire(m) => {
                m.center = m.center.to_mm(unit);
                m.diameter = m.diameter.to_mm(unit);
                m.cross_hair_length = m.cross_hair_length.to_mm(unit);
                m.cross_hair_thickness = m.cross_hair_thickness.to_mm(unit);
                m.gap = m.gap.to_mm(unit);
                m.ring_thickness = m.ring_thickness.to_mm(unit);
            }
            MacroContent::Thermal(t) => {
                t.center = t.center.to_mm(unit);
                t.gap = t.gap.to_mm(unit);
                t.inner_diameter = t.inner_diameter.to_mm(unit);
                t.outer_diameter.to_mm(unit);
            }
            MacroContent::VariableDefinition(_) | MacroContent::Comment(_) => {}
        }
        cloned
    }
}

impl UnitAble for Aperture {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            Aperture::Circle(circle) => {
                circle.diameter = circle.diameter.mm_to_unit(unit);
                circle.hole_diameter = circle.hole_diameter.mm_to_unit(unit);
            }
            Aperture::Obround(rect) | Aperture::Rectangle(rect) => {
                rect.x = rect.x.mm_to_unit(unit);
                rect.x = rect.y.mm_to_unit(unit);
                rect.hole_diameter = rect.hole_diameter.mm_to_unit(unit);
            }
            Aperture::Polygon(poly) => {
                poly.diameter = poly.diameter.mm_to_unit(unit);
                poly.hole_diameter = poly.hole_diameter.mm_to_unit(unit);
            }
            // TODO: whatever we need to do here.
            Aperture::Macro(_, Some(_)) => {}
            Aperture::Macro(_, None) => {}
        };
        cloned
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            Aperture::Circle(circle) => {
                circle.diameter = circle.diameter.to_mm(unit);
                circle.hole_diameter = circle.hole_diameter.to_mm(unit);
            }
            Aperture::Obround(rect) | Aperture::Rectangle(rect) => {
                rect.x = rect.x.to_mm(unit);
                rect.x = rect.y.to_mm(unit);
                rect.hole_diameter = rect.hole_diameter.to_mm(unit);
            }
            Aperture::Polygon(poly) => {
                poly.diameter = poly.diameter.to_mm(unit);
                poly.hole_diameter = poly.hole_diameter.to_mm(unit);
            }
            // TODO: whatever we need to do here.
            Aperture::Macro(_, Some(_)) => {}
            Aperture::Macro(_, None) => {}
        }
        cloned
    }
}

impl UnitAble for Operation {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            Operation::Interpolate(Some(val), _)
            | Operation::Move(Some(val))
            | Operation::Flash(Some(val)) => {
                val.x = val.x.mm_to_unit(unit);
            }
            _ => {}
        };
        if let Operation::Interpolate(_, Some(val)) = &mut cloned {
            val.y = val.y.mm_to_unit(unit);
        }
        cloned
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            Operation::Interpolate(Some(val), _)
            | Operation::Move(Some(val))
            | Operation::Flash(Some(val)) => {
                val.x = val.x.to_mm(unit);
            }
            _ => {}
        };
        if let Operation::Interpolate(_, Some(val)) = &mut cloned {
            val.y = val.y.to_mm(unit);
        }
        cloned
    }
}

impl UnitAble for DCode {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        match self {
            DCode::Operation(val) => DCode::Operation(val.mm_to_unit(unit)),
            DCode::SelectAperture(val) => DCode::SelectAperture(*val),
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        match self {
            DCode::Operation(val) => DCode::Operation(val.to_mm(unit)),
            DCode::SelectAperture(val) => DCode::SelectAperture(*val),
        }
    }
}

impl UnitAble for FunctionCode {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        match self {
            FunctionCode::DCode(val) => FunctionCode::DCode(val.mm_to_unit(unit)),
            val => val.clone(),
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        match self {
            FunctionCode::DCode(val) => FunctionCode::DCode(val.to_mm(unit)),
            val => val.clone(),
        }
    }
}

impl UnitAble for ApertureDefinition {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        cloned.aperture = cloned.aperture.mm_to_unit(unit);
        cloned
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        cloned.aperture = cloned.aperture.to_mm(unit);
        cloned
    }
}

impl UnitAble for StepAndRepeat {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            StepAndRepeat::Open {
                distance_x,
                distance_y,
                ..
            } => {
                *distance_x = distance_x.mm_to_unit(unit);
                *distance_y = distance_y.mm_to_unit(unit);
            }
            StepAndRepeat::Close => {}
        }
        cloned
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            StepAndRepeat::Open {
                distance_x,
                distance_y,
                ..
            } => {
                *distance_x = distance_x.to_mm(unit);
                *distance_y = distance_y.to_mm(unit);
            }
            StepAndRepeat::Close => {}
        }
        cloned
    }
}

impl UnitAble for ApertureMacro {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        for content in &mut cloned.content {
            *content = content.mm_to_unit(unit);
        }
        cloned
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        let mut cloned = self.clone();
        for content in &mut cloned.content {
            *content = content.to_mm(unit);
        }
        cloned
    }
}
impl UnitAble for ExtendedCode {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        match self {
            ExtendedCode::ApertureDefinition(ap) => {
                ExtendedCode::ApertureDefinition(ap.mm_to_unit(unit))
            }
            ExtendedCode::ApertureMacro(val) => ExtendedCode::ApertureMacro(val.mm_to_unit(unit)),
            ExtendedCode::StepAndRepeat(val) => ExtendedCode::StepAndRepeat(val.mm_to_unit(unit)),
            val => val.clone(),
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        match self {
            ExtendedCode::ApertureDefinition(ap) => {
                ExtendedCode::ApertureDefinition(ap.to_mm(unit))
            }
            ExtendedCode::ApertureMacro(val) => ExtendedCode::ApertureMacro(val.to_mm(unit)),
            ExtendedCode::StepAndRepeat(val) => ExtendedCode::StepAndRepeat(val.to_mm(unit)),
            val => val.clone(),
        }
    }
}

impl UnitAble for Command {
    fn mm_to_unit(&self, unit: &Unit) -> Self {
        match self {
            Command::FunctionCode(code) => Command::FunctionCode(code.mm_to_unit(unit)),
            Command::ExtendedCode(code) => Command::ExtendedCode(code.mm_to_unit(unit)),
        }
    }

    fn to_mm(&self, unit: &Unit) -> Self {
        match self {
            Command::FunctionCode(code) => Command::FunctionCode(code.to_mm(unit)),
            Command::ExtendedCode(code) => Command::ExtendedCode(code.to_mm(unit)),
        }
    }
}
