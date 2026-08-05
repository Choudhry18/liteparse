//! VML formula parsing.

use crate::docx::model::*;

/// Parse a single VML formula equation string (e.g., "sum #0 0 10800").
pub(super) fn parse_formula(eqn: &str) -> Option<VmlFormula> {
    let parts: Vec<&str> = eqn.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let operation = match parts[0] {
        "val" => VmlFormulaOp::Val,
        "sum" => VmlFormulaOp::Sum,
        "prod" => VmlFormulaOp::Product,
        "mid" => VmlFormulaOp::Mid,
        "abs" => VmlFormulaOp::Abs,
        "min" => VmlFormulaOp::Min,
        "max" => VmlFormulaOp::Max,
        "if" => VmlFormulaOp::If,
        "sqrt" => VmlFormulaOp::Sqrt,
        "mod" => VmlFormulaOp::Mod,
        "sin" => VmlFormulaOp::Sin,
        "cos" => VmlFormulaOp::Cos,
        "tan" => VmlFormulaOp::Tan,
        "atan2" => VmlFormulaOp::Atan2,
        "sinatan2" => VmlFormulaOp::SinAtan2,
        "cosatan2" => VmlFormulaOp::CosAtan2,
        "sumangle" => VmlFormulaOp::SumAngle,
        "ellipse" => VmlFormulaOp::Ellipse,
        other => {
            log::warn!("vml-formula: unsupported operation {:?}", other);
            return None;
        }
    };

    let arg = |i: usize| -> VmlFormulaArg {
        parts
            .get(i)
            .and_then(|s| parse_formula_arg(s))
            .unwrap_or(VmlFormulaArg::Literal(0))
    };

    Some(VmlFormula {
        operation,
        args: [arg(1), arg(2), arg(3)],
    })
}

/// Parse a single VML formula argument.
fn parse_formula_arg(s: &str) -> Option<VmlFormulaArg> {
    if let Some(rest) = s.strip_prefix('#') {
        return rest.parse::<u32>().ok().map(VmlFormulaArg::AdjRef);
    }
    if let Some(rest) = s.strip_prefix('@') {
        return rest.parse::<u32>().ok().map(VmlFormulaArg::FormulaRef);
    }
    let guide = match s {
        "width" => Some(VmlGuide::Width),
        "height" => Some(VmlGuide::Height),
        "xcenter" => Some(VmlGuide::XCenter),
        "ycenter" => Some(VmlGuide::YCenter),
        "xrange" => Some(VmlGuide::XRange),
        "yrange" => Some(VmlGuide::YRange),
        "pixelWidth" => Some(VmlGuide::PixelWidth),
        "pixelHeight" => Some(VmlGuide::PixelHeight),
        "pixelLineWidth" => Some(VmlGuide::PixelLineWidth),
        "emuWidth" => Some(VmlGuide::EmuWidth),
        "emuHeight" => Some(VmlGuide::EmuHeight),
        "emuWidth2" => Some(VmlGuide::EmuWidth2),
        "emuHeight2" => Some(VmlGuide::EmuHeight2),
        _ => None,
    };
    if let Some(g) = guide {
        return Some(VmlFormulaArg::Guide(g));
    }
    s.parse::<i64>().ok().map(VmlFormulaArg::Literal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_with_adj_ref_and_literals() {
        let f = parse_formula("sum #0 0 10800").unwrap();
        assert_eq!(f.operation, VmlFormulaOp::Sum);
        assert_eq!(
            f.args,
            [
                VmlFormulaArg::AdjRef(0),
                VmlFormulaArg::Literal(0),
                VmlFormulaArg::Literal(10800),
            ]
        );
    }

    #[test]
    fn prod_maps_from_short_name() {
        let f = parse_formula("prod @2 1 2").unwrap();
        assert_eq!(f.operation, VmlFormulaOp::Product);
        assert_eq!(f.args[0], VmlFormulaArg::FormulaRef(2));
    }

    #[test]
    fn guide_arguments() {
        let f = parse_formula("mid width height").unwrap();
        assert_eq!(f.operation, VmlFormulaOp::Mid);
        assert_eq!(f.args[0], VmlFormulaArg::Guide(VmlGuide::Width));
        assert_eq!(f.args[1], VmlFormulaArg::Guide(VmlGuide::Height));
        // No third token → defaults to literal 0.
        assert_eq!(f.args[2], VmlFormulaArg::Literal(0));
    }

    #[test]
    fn missing_arguments_default_to_literal_zero() {
        let f = parse_formula("val 100").unwrap();
        assert_eq!(f.operation, VmlFormulaOp::Val);
        assert_eq!(
            f.args,
            [
                VmlFormulaArg::Literal(100),
                VmlFormulaArg::Literal(0),
                VmlFormulaArg::Literal(0),
            ]
        );
    }

    #[test]
    fn negative_literal() {
        let f = parse_formula("sum 0 0 -10800").unwrap();
        assert_eq!(f.args[2], VmlFormulaArg::Literal(-10800));
    }

    #[test]
    fn unknown_operation_is_dropped() {
        assert!(parse_formula("bogus 1 2 3").is_none());
    }

    #[test]
    fn empty_string_is_none() {
        assert!(parse_formula("").is_none());
        assert!(parse_formula("   ").is_none());
    }

    #[test]
    fn unparseable_guide_falls_back_to_literal_zero() {
        // An unrecognized non-numeric arg is not a guide and not an integer;
        // `arg()` substitutes literal 0 rather than failing the whole formula.
        let f = parse_formula("sum notaguide 5 6").unwrap();
        assert_eq!(f.args[0], VmlFormulaArg::Literal(0));
        assert_eq!(f.args[1], VmlFormulaArg::Literal(5));
    }
}
