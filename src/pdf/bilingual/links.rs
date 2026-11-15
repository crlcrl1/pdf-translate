//! Carry link hit areas and destinations from each source into its output half.
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use hayro::kurbo::{Affine, Point, Rect};
use lopdf::{Dictionary, Document, Object, ObjectId};

const MAX_DEPTH: usize = 64;
type NamedDestinations = HashMap<Vec<u8>, Object>;

pub(super) struct PagePlacement {
    pub page: ObjectId,
    pub transform: Affine,
    pub bounds: Rect,
}

/// Keep the full destination arrays: lopdf's outline-oriented helper retains
/// only the page and view type, losing the coordinates needed for links.
pub(super) fn named_destinations(document: &Document) -> Result<NamedDestinations> {
    let catalog = document.catalog()?;
    let mut destinations = HashMap::new();
    if let Ok(legacy) = catalog
        .get_deref(b"Dests", document)
        .and_then(Object::as_dict)
    {
        destinations.extend(
            legacy
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    let tree = catalog
        .get_deref(b"Names", document)
        .and_then(Object::as_dict)
        .and_then(|names| names.get(b"Dests"));
    let Ok(tree) = tree else {
        return Ok(destinations);
    };
    let mut pending = vec![(tree, 0)];
    let mut visited = HashSet::new();
    while let Some((node, depth)) = pending.pop() {
        let (id, node) = document.dereference(node)?;
        if id.is_some_and(|id| !visited.insert(id)) {
            continue;
        }
        if depth >= MAX_DEPTH {
            bail!("destination name tree is too deep");
        }
        let node = node.as_dict()?;
        if let Ok(names) = node
            .get_deref(b"Names", document)
            .and_then(Object::as_array)
        {
            for pair in names.as_chunks::<2>().0 {
                if let Ok(name) = pair[0].as_str() {
                    destinations.insert(name.to_vec(), pair[1].clone());
                }
            }
        }
        if let Ok(kids) = node.get_deref(b"Kids", document).and_then(Object::as_array) {
            pending.extend(kids.iter().map(|kid| (kid, depth + 1)));
        }
    }
    Ok(destinations)
}

pub(super) fn copy_page_links(
    document: &mut Document,
    source: ObjectId,
    placements: &HashMap<ObjectId, PagePlacement>,
    names: &NamedDestinations,
) -> Result<Vec<Object>> {
    let placement = &placements[&source];
    let mut links = Vec::new();
    for annotation in document.get_page_annotations(source)? {
        if !annotation
            .get_deref(b"Subtype", document)
            .and_then(Object::as_name)
            .is_ok_and(|subtype| subtype == b"Link")
        {
            continue;
        }
        match remap_link(document, annotation, placement, placements, names) {
            Ok(link) => links.push(link),
            Err(error) => tracing::warn!(?source, %error, "skipping an invalid PDF link"),
        }
    }
    Ok(links
        .into_iter()
        .map(|link| document.add_object(link).into())
        .collect())
}

fn remap_link(
    document: &Document,
    annotation: &Dictionary,
    placement: &PagePlacement,
    placements: &HashMap<ObjectId, PagePlacement>,
    names: &NamedDestinations,
) -> Result<Dictionary> {
    let mut link = annotation.clone();
    let rect = read_rect(document, annotation.get(b"Rect")?)?;
    link.set(
        "Rect",
        rect_object(placement.transform.transform_rect_bbox(rect)),
    );
    link.set("P", placement.page);
    // The combined document has no structure tree from either source.
    link.remove(b"StructParent");
    if let Ok(points) = annotation
        .get_deref(b"QuadPoints", document)
        .and_then(Object::as_array)
    {
        if !points.is_empty() && points.len() % 8 == 0 {
            let mut transformed = Vec::with_capacity(points.len());
            for pair in points.as_chunks::<2>().0 {
                let point = placement.transform
                    * Point::new(number(document, &pair[0])?, number(document, &pair[1])?);
                transformed.extend([Object::from(point.x), Object::from(point.y)]);
            }
            link.set("QuadPoints", transformed);
        } else {
            link.remove(b"QuadPoints");
        }
    }
    if let Ok(dest) = annotation.get(b"Dest") {
        link.set(
            "Dest",
            remap_destination(document, dest, placements, names)?,
        );
    }
    if let Ok(action) = annotation.get(b"A") {
        link.set("A", remap_action(document, action, placements, names, 0)?);
    }
    Ok(link)
}

fn remap_action(
    document: &Document,
    action: &Object,
    placements: &HashMap<ObjectId, PagePlacement>,
    names: &NamedDestinations,
    depth: usize,
) -> Result<Object> {
    if depth >= MAX_DEPTH {
        bail!("link action chain is too deep or cyclic");
    }
    let action = document.dereference(action)?.1;
    if let Object::Array(actions) = action {
        return Ok(Object::Array(
            actions
                .iter()
                .map(|action| remap_action(document, action, placements, names, depth + 1))
                .collect::<Result<_>>()?,
        ));
    }
    let mut action = action.as_dict()?.clone();
    if action.get_deref(b"S", document)?.as_name()? == b"GoTo" {
        action.set(
            "D",
            remap_destination(document, action.get(b"D")?, placements, names)?,
        );
    }
    // URI and remote GoToR destinations belong to other documents and must not
    // be transformed. A following local GoTo still needs our new page IDs.
    if let Ok(next) = action.get(b"Next") {
        action.set(
            "Next",
            remap_action(document, next, placements, names, depth + 1)?,
        );
    }
    Ok(Object::Dictionary(action))
}

fn remap_destination(
    document: &Document,
    destination: &Object,
    placements: &HashMap<ObjectId, PagePlacement>,
    names: &NamedDestinations,
) -> Result<Object> {
    let mut destination = destination;
    for _ in 0..MAX_DEPTH {
        destination = document.dereference(destination)?.1;
        match destination {
            Object::Array(values) => {
                return remap_explicit_destination(document, values, placements);
            }
            Object::Name(name) | Object::String(name, _) => {
                destination = names.get(name).context("missing named link destination")?;
            }
            Object::Dictionary(dict) => destination = dict.get(b"D")?,
            _ => bail!("invalid link destination"),
        }
    }
    bail!("link destination is too deep or cyclic")
}

fn remap_explicit_destination(
    document: &Document,
    values: &[Object],
    placements: &HashMap<ObjectId, PagePlacement>,
) -> Result<Object> {
    let [page, view, rest @ ..] = values else {
        bail!("incomplete link destination")
    };
    let placement = placements
        .get(&page.as_reference()?)
        .context("link target is not an output page")?;
    let view = document.dereference(view)?.1.as_name()?;
    let mut target = vec![placement.page.into()];
    match (view, rest) {
        (b"XYZ", [x, y, zoom]) => {
            let [x, y] = transform_optional_point(document, placement.transform, x, y)?;
            target.extend(["XYZ".into(), x, y, zoom.clone()]);
        }
        (b"Fit" | b"FitB", []) => {
            target.push("FitR".into());
            target.extend(rect_object(placement.bounds));
        }
        (b"FitR", [_, _, _, _]) => {
            let rect = read_rect(document, &Object::Array(rest.to_vec()))?;
            target.push("FitR".into());
            target.extend(rect_object(placement.transform.transform_rect_bbox(rect)));
        }
        (b"FitH" | b"FitBH" | b"FitV" | b"FitBV", [coordinate]) => {
            // Fitting the new page width would include both halves. Preserve
            // the target's anchor on its own half and retain the current zoom.
            let crop = placement
                .transform
                .inverse()
                .transform_rect_bbox(placement.bounds);
            let (x, y) = if matches!(view, b"FitH" | b"FitBH") {
                (Object::from(crop.x0), coordinate.clone())
            } else {
                (coordinate.clone(), Object::from(crop.y1))
            };
            let [x, y] = transform_optional_point(document, placement.transform, &x, &y)?;
            target.extend(["XYZ".into(), x, y, Object::Null]);
        }
        _ => bail!("invalid destination view"),
    }
    Ok(Object::Array(target))
}

fn transform_optional_point(
    document: &Document,
    transform: Affine,
    x: &Object,
    y: &Object,
) -> Result<[Object; 2]> {
    let [a, b, c, d, e, f] = transform.as_coeffs();
    let coordinate = |cx: f64, cy: f64, offset: f64| -> Result<Object> {
        let mut value = offset;
        for (coefficient, object) in [(cx, x), (cy, y)] {
            if coefficient != 0.0 {
                if document.dereference(object)?.1.is_null() {
                    return Ok(Object::Null);
                }
                value += coefficient * number(document, object)?;
            }
        }
        Ok(value.into())
    };
    Ok([coordinate(a, c, e)?, coordinate(b, d, f)?])
}

fn number(document: &Document, object: &Object) -> Result<f64> {
    let value = f64::from(document.dereference(object)?.1.as_float()?);
    if !value.is_finite() {
        bail!("non-finite link coordinate")
    }
    Ok(value)
}

fn read_rect(document: &Document, object: &Object) -> Result<Rect> {
    let values = document.dereference(object)?.1.as_array()?;
    let [x0, y0, x1, y1] = values.as_slice() else {
        bail!("link rectangle must have four numbers")
    };
    Ok(Rect::new(
        number(document, x0)?,
        number(document, y0)?,
        number(document, x1)?,
        number(document, y1)?,
    )
    .abs())
}

fn rect_object(rect: Rect) -> Vec<Object> {
    [rect.x0, rect.y0, rect.x1, rect.y1]
        .map(Object::from)
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn destination_views_and_null_coordinates_stay_on_the_correct_half() {
        let doc = Document::new();
        let placements = HashMap::from([(
            (1, 0),
            PagePlacement {
                page: (10, 0),
                transform: Affine::translate((624.0, 0.0)),
                bounds: Rect::new(624.0, 0.0, 1236.0, 792.0),
            },
        )]);
        for (view, values, expected) in [
            (
                "Fit",
                vec![],
                vec!["FitR".into(), 624.into(), 0.into(), 1236.into(), 792.into()],
            ),
            (
                "FitB",
                vec![],
                vec!["FitR".into(), 624.into(), 0.into(), 1236.into(), 792.into()],
            ),
            (
                "FitR",
                vec![70.into(), 500.into(), 100.into(), 600.into()],
                vec![
                    "FitR".into(),
                    694.into(),
                    500.into(),
                    724.into(),
                    600.into(),
                ],
            ),
            (
                "FitH",
                vec![650.into()],
                vec!["XYZ".into(), 624.into(), 650.into(), Object::Null],
            ),
            (
                "FitBH",
                vec![Object::Null],
                vec!["XYZ".into(), 624.into(), Object::Null, Object::Null],
            ),
            (
                "FitV",
                vec![70.into()],
                vec!["XYZ".into(), 694.into(), 792.into(), Object::Null],
            ),
            (
                "FitBV",
                vec![70.into()],
                vec!["XYZ".into(), 694.into(), 792.into(), Object::Null],
            ),
            (
                "XYZ",
                vec![Object::Null, 650.into(), 0.into()],
                vec!["XYZ".into(), Object::Null, 650.into(), 0.into()],
            ),
        ] {
            let mut input = vec![Object::Reference((1, 0)), view.into()];
            input.extend(values);
            let output = remap_explicit_destination(&doc, &input, &placements).unwrap();
            let output = output.as_array().unwrap();
            assert_eq!(output[0].as_reference().unwrap(), (10, 0));
            for (actual, expected) in output[1..].iter().zip(expected) {
                if let Ok(number) = expected.as_float() {
                    assert_eq!(actual.as_float().unwrap(), number, "{view}");
                } else {
                    assert_eq!(actual, &expected, "{view}");
                }
            }
        }
        let rotation = Affine::new([0., -1., 1., 0., -30., 592.]);
        assert_eq!(
            transform_optional_point(&doc, rotation, &Object::Null, &650.into()).unwrap(),
            [620.0.into(), Object::Null]
        );
    }

    #[test]
    fn remote_actions_are_preserved_and_cyclic_local_destinations_are_rejected() {
        let mut doc = Document::new();
        let mut placements = HashMap::new();
        placements.insert(
            (1, 0),
            PagePlacement {
                page: (10, 0),
                transform: Affine::translate((624., 0.)),
                bounds: Rect::new(624., 0., 1236., 792.),
            },
        );
        let remote = dictionary! {
            "S" => "GoToR", "F" => Object::string_literal("other.pdf"),
            "D" => vec![0.into(), "XYZ".into(), 70.into(), 650.into(), Object::Null],
            "Next" => vec![Object::Dictionary(dictionary! {
                "S" => "GoTo", "D" => vec![Object::Reference((1, 0)), "Fit".into()],
            })],
        };
        let output = remap_action(
            &doc,
            &remote.clone().into(),
            &placements,
            &HashMap::new(),
            0,
        )
        .unwrap();
        let output = output.as_dict().unwrap();
        assert_eq!(output.get(b"D").unwrap(), remote.get(b"D").unwrap());
        assert_eq!(output.get(b"F").unwrap(), remote.get(b"F").unwrap());
        let next = output.get(b"Next").unwrap().as_array().unwrap()[0]
            .as_dict()
            .unwrap();
        assert_eq!(
            next.get(b"D").unwrap().as_array().unwrap()[0]
                .as_reference()
                .unwrap(),
            (10, 0)
        );

        let names = HashMap::from([(b"cycle".to_vec(), Object::string_literal("cycle"))]);
        assert!(
            remap_destination(&doc, &Object::string_literal("cycle"), &placements, &names).is_err()
        );
        let action_id = doc.add_object(
            dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.org") },
        );
        doc.get_dictionary_mut(action_id)
            .unwrap()
            .set("Next", action_id);
        assert!(remap_action(&doc, &action_id.into(), &placements, &names, 0).is_err());
    }
}
