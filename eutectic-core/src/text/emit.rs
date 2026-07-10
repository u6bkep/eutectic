//! Pure canonical formatters (point/path/length/role) and the orientation codec
//! helpers (parse+render round-trip) shared by the render and parse paths.

use super::*;

pub(crate) fn fmt_point(p: Point) -> String {
    format!("({}, {})", fmt_len(p.x), fmt_len(p.y))
}

/// Render a skeleton [`Path`] as a coordinate list: `start`, then one coordinate per
/// straight edge, and `arc <mid> <end>` per circular-arc edge. The inverse of
/// [`extract_path`]. (The closing edge of a polygon is implicit, as in the geometry.)
pub(crate) fn fmt_path(path: &Path) -> String {
    let mut toks = vec![fmt_point(path.start)];
    for seg in &path.segs {
        match seg {
            Seg::Line { end } => toks.push(fmt_point(*end)),
            Seg::Arc { mid, end } => {
                toks.push("arc".into());
                toks.push(fmt_point(*mid));
                toks.push(fmt_point(*end));
            }
            Seg::Quadratic { ctrl, end } => {
                toks.push("quad".into());
                toks.push(fmt_point(*ctrl));
                toks.push(fmt_point(*end));
            }
            Seg::Cubic { c1, c2, end } => {
                toks.push("cubic".into());
                toks.push(fmt_point(*c1));
                toks.push(fmt_point(*c2));
                toks.push(fmt_point(*end));
            }
        }
    }
    toks.join(" ")
}

/// Canonical length rendering: always millimetres. Whole-mm values print without a
/// fraction (`30mm`); otherwise the minimal exact decimal is emitted (`0.5mm`,
/// `0.000001mm` for a single nm). Exact for any `i64` nm — no float involved.
pub(crate) fn fmt_len(v: Nm) -> String {
    if v % MM == 0 {
        return format!("{}mm", v / MM);
    }
    let neg = v < 0;
    let a = v.unsigned_abs();
    let whole = a / MM as u64;
    let frac = a % MM as u64;
    let frac6 = format!("{frac:06}");
    let trimmed = frac6.trim_end_matches('0');
    format!("{}{whole}.{trimmed}mm", if neg { "-" } else { "" })
}

/// Canonical text token for a region [`Role`]. Only the roles a `region` directive can
/// author round-trip here (conductor / void / keep-out by kind); other roles are
/// composed via footprints, not authored as standalone regions.
pub(crate) fn role_token(role: &Role) -> String {
    match role {
        Role::Conductor => "conductor".into(),
        Role::Void => "void".into(),
        Role::Keepout(k) => match k {
            KeepoutKind::Copper => "keepout".into(),
            KeepoutKind::Component => "keepout-component".into(),
            KeepoutKind::Drill => "keepout-drill".into(),
            KeepoutKind::Route => "keepout-route".into(),
        },
        // Not authorable via a `region` directive (the `region` parser rejects these),
        // but they ARE authorable as `slab` roles and round-trip that way — so the
        // token must stay stable and lossless. `substrate`/`marking`/`mask`/`datum`
        // are all parsed by the `slab` grammar.
        Role::Substrate => "substrate".into(),
        Role::Marking => "marking".into(),
        Role::Mask => "mask".into(),
        Role::Datum => "datum".into(),
    }
}

pub(crate) fn parse_role(tok: &str) -> Result<Role, String> {
    Ok(match tok {
        "conductor" => Role::Conductor,
        "void" => Role::Void,
        "keepout" => Role::Keepout(KeepoutKind::Copper),
        "keepout-component" => Role::Keepout(KeepoutKind::Component),
        "keepout-drill" => Role::Keepout(KeepoutKind::Drill),
        "keepout-route" => Role::Keepout(KeepoutKind::Route),
        other => {
            return Err(format!(
                "region: unknown role `{other}` (conductor | void | keepout[-component|-drill|-route])"
            ));
        }
    })
}

/// Parse a `rot=` degree value into an [`Orient`] (about z): an integer cardinal uses
/// the tiny exact quaternion, any other finite angle lowers once (at parse) to a scaled
/// quaternion (same lowering as the `rotate` directive). Mirrors that directive's angle
/// handling; text-side flipping (`bottom`) is a follow-up.
pub(crate) fn parse_rot_deg(r: &str) -> Result<Orient, String> {
    if let Ok(d) = r.parse::<i32>() {
        Ok(Orient::from_deg(d).unwrap_or_else(|| Orient::from_angle_deg(d as f64)))
    } else {
        let deg: f64 = r
            .parse()
            .map_err(|_| format!("`{r}` is not a number of degrees"))?;
        if !deg.is_finite() {
            return Err(format!("rotation angle `{r}` must be finite"));
        }
        Ok(Orient::from_angle_deg(deg))
    }
}

/// Parse a `rotq=` value `(w,x,y,z)` into an exact integer-quaternion [`Orient`] (the
/// canonical serialised form for a non-cardinal text rotation). The all-zero quaternion
/// is rejected (not a rotation).
pub(crate) fn parse_quat_tok(q: &str) -> Result<Orient, String> {
    let inner = q
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or("rotq must be written rotq=(w,x,y,z)")?;
    let n: Vec<&str> = inner.split(',').collect();
    if n.len() != 4 {
        return Err("rotq needs four integer components: rotq=(w,x,y,z)".into());
    }
    let pi = |t: &str| {
        t.trim()
            .parse::<i64>()
            .map_err(|_| format!("`{}` is not an integer", t.trim()))
    };
    let o = Orient {
        w: pi(n[0])?,
        x: pi(n[1])?,
        y: pi(n[2])?,
        z: pi(n[3])?,
    };
    if (o.w, o.x, o.y, o.z) == (0, 0, 0, 0) {
        return Err("rotq=(0,0,0,0) is not a rotation".into());
    }
    Ok(o)
}
