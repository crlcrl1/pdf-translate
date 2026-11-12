use super::{
    MAX_DEPTH, MAX_FORM_INVOCATIONS, MAX_TEXT_OPERATIONS, Plan, Step, TRACE_TAG, resolved_dict,
};
use crate::pdf::content::MAX_CONTENT_STREAM_BYTES;
use anyhow::{Context, Result, bail, ensure};
use lopdf::{
    Dictionary, Document, Object, ObjectId, Stream,
    content::{Content, Operation},
    dictionary,
};
use std::collections::HashSet;

pub(super) fn probe_font(doc: &mut Document) -> ObjectId {
    let glyph = doc.add_object(Stream::new(dictionary! {}, b"0 0 d0".to_vec()));
    doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type3", "Name" => "CursorProbe",
        "FontBBox" => vec![0.into(),0.into(),1.into(),1.into()],
        "FontMatrix" => vec![0.001.into(),0.into(),0.into(),0.001.into(),0.into(),0.into()],
        "FirstChar" => 0, "LastChar" => 0, "Widths" => vec![0.into()],
        "Encoding" => dictionary! {"Type" => "Encoding", "Differences" => vec![0.into(), Object::Name(b"probe".to_vec())]},
        "CharProcs" => dictionary! {"probe" => glyph}, "Resources" => dictionary! {},
    })
}

pub(super) struct Instrumenter<'a> {
    doc: &'a mut Document,
    probe: ObjectId,
    pub calls: usize,
    pub plans: Vec<Plan>,
    visiting: HashSet<ObjectId>,
    forms: usize,
    graphic_forms: HashSet<ObjectId>,
}

#[derive(Clone, Copy)]
struct Location {
    page: usize,
    stream: ObjectId,
    depth: usize,
    rise: f64,
}

struct BuiltStream {
    bytes: Vec<u8>,
    resources: Dictionary,
}

impl<'a> Instrumenter<'a> {
    pub fn new(doc: &'a mut Document, probe: ObjectId) -> Self {
        Self {
            doc,
            probe,
            calls: 0,
            plans: Vec::new(),
            visiting: HashSet::new(),
            forms: 0,
            graphic_forms: HashSet::new(),
        }
    }

    pub fn page(&mut self, page: usize, page_id: ObjectId) -> Result<()> {
        let (bytes, resources) = crate::pdf::content::page_original_content(self.doc, page_id)?;
        let stream = self.doc.new_object_id();
        let built = self.stream(
            &bytes,
            resources,
            Location {
                page,
                stream,
                depth: 0,
                rise: 0.0,
            },
        )?;
        self.doc
            .objects
            .insert(stream, Stream::new(dictionary! {}, built.bytes).into());
        let dict = self.doc.get_dictionary_mut(page_id)?;
        dict.set("Contents", stream);
        dict.set("Resources", built.resources);
        Ok(())
    }

    fn stream(
        &mut self,
        bytes: &[u8],
        mut resources: Dictionary,
        loc: Location,
    ) -> Result<BuiltStream> {
        ensure!(
            loc.depth <= MAX_DEPTH,
            "Form XObject nesting exceeds {MAX_DEPTH}"
        );
        let original_xobjects =
            resolved_dict(self.doc, resources.get(b"XObject").ok()).unwrap_or_default();
        let mut xobjects = Dictionary::new();
        // Keep non-Form resources: image names may also be used by Type3 fonts.
        for (name, object) in &original_xobjects {
            let is_form = object
                .as_reference()
                .ok()
                .and_then(|id| self.doc.get_object(id).ok())
                .and_then(|o| o.as_stream().ok())
                .is_some_and(|s| {
                    s.dict
                        .get(b"Subtype")
                        .and_then(Object::as_name)
                        .is_ok_and(|n| n == b"Form")
                });
            if !is_form {
                xobjects.set(name.clone(), object.clone());
            }
        }
        let mut fonts = resolved_dict(self.doc, resources.get(b"Font").ok()).unwrap_or_default();
        let mut probe_name = String::from("PdfTranslateCursor");
        while fonts.has(probe_name.as_bytes()) {
            probe_name.push('_');
        }
        fonts.set(probe_name.clone(), self.probe);
        resources.set("Font", fonts);
        let mut rise = loc.rise;
        let mut saved = Vec::new();
        let mut scopes = Vec::new();
        let mut steps = Vec::new();
        let mut instrumented = Vec::new();
        let original = Content::decode(bytes)?;
        for op in original.operations {
            for mut op in expand_quotes(op)? {
                let child = if op.operator == "Do" {
                    self.instrument_form(
                        &mut op,
                        &original_xobjects,
                        &mut xobjects,
                        &resources,
                        loc,
                        rise,
                    )?
                } else {
                    None
                };
                if matches!(op.operator.as_str(), "BDC" | "BMC")
                    && op.operands.first().and_then(|o| o.as_name().ok()) == Some(TRACE_TAG)
                {
                    bail!("input already uses the internal rewrite marker");
                }
                match op.operator.as_str() {
                    "q" => saved.push(rise),
                    "Q" => rise = saved.pop().unwrap_or(loc.rise),
                    "Ts" => {
                        rise = f64::from(
                            op.operands
                                .first()
                                .context("missing Ts operand")?
                                .as_float()?,
                        )
                    }
                    "BDC" | "BMC" => scopes.push(steps.len()),
                    "EMC" => {
                        scopes.pop();
                    }
                    _ => {}
                }
                let call = if matches!(op.operator.as_str(), "Tj" | "TJ") {
                    ensure!(
                        self.calls < MAX_TEXT_OPERATIONS,
                        "too many text operations to rewrite safely (limit {MAX_TEXT_OPERATIONS})"
                    );
                    let id = self.calls;
                    self.calls += 1;
                    append_probe(&mut instrumented, &op, &probe_name, id);
                    Some(id)
                } else if op.operator == "S"
                    || op.operator == "BI"
                    || (op.operator == "Do"
                        && op
                            .operands
                            .first()
                            .and_then(|o| o.as_name().ok())
                            .and_then(|name| original_xobjects.get(name).ok())
                            .and_then(|o| o.as_reference().ok())
                            .and_then(|id| self.doc.get_object(id).ok())
                            .and_then(|o| o.as_stream().ok())
                            .is_some_and(|stream| {
                                stream
                                    .dict
                                    .get(b"Subtype")
                                    .and_then(Object::as_name)
                                    .is_ok_and(|name| name == b"Image")
                            }))
                {
                    ensure!(
                        self.calls < MAX_TEXT_OPERATIONS,
                        "too many operations to rewrite safely"
                    );
                    let id = self.calls;
                    self.calls += 1;
                    instrumented.push(Operation::new(
                        "BDC",
                        vec![
                            Object::Name(TRACE_TAG.to_vec()),
                            dictionary! {"Id" => id as i64, "Probe" => false}.into(),
                        ],
                    ));
                    instrumented.push(op.clone());
                    instrumented.push(Operation::new("EMC", vec![]));
                    Some(id)
                } else {
                    instrumented.push(op.clone());
                    None
                };
                steps.push(Step {
                    operation: op,
                    call,
                    rise,
                    scopes: scopes.clone(),
                    child,
                });
            }
        }
        resources.set("XObject", xobjects);
        self.plans.push(Plan {
            page: loc.page,
            stream: loc.stream,
            steps,
            resources: resources.clone(),
            is_page: loc.depth == 0,
        });
        Ok(BuiltStream {
            bytes: Content {
                operations: instrumented,
            }
            .encode()?,
            resources,
        })
    }

    fn instrument_form(
        &mut self,
        op: &mut Operation,
        original_xobjects: &Dictionary,
        xobjects: &mut Dictionary,
        resources: &Dictionary,
        loc: Location,
        rise: f64,
    ) -> Result<Option<ObjectId>> {
        let name = op
            .operands
            .first()
            .and_then(|o| o.as_name().ok())
            .context("invalid XObject invocation")?;
        let Ok(object) = original_xobjects.get(name) else {
            return Ok(None);
        };
        let id = object.as_reference()?;
        let source = self.doc.get_object(id)?.as_stream()?;
        if !source
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|n| n == b"Form")
        {
            xobjects.set(name.to_vec(), object.clone());
            return Ok(None);
        }
        let source = source.clone();
        // Plot markers can invoke one text-free Form tens of thousands of
        // times. Keep those leaves shared; only text-bearing invocations need
        // independent rewrite plans. Do not skip nested Forms or Type3 text.
        let data = source.get_plain_content_with_limit(MAX_CONTENT_STREAM_BYTES)?;
        if self.graphic_forms.contains(&id)
            || !Content::decode(&data)?
                .operations
                .iter()
                .any(|op| matches!(op.operator.as_str(), "Tj" | "TJ" | "'" | "\"" | "Do"))
        {
            self.graphic_forms.insert(id);
            xobjects.set(name.to_vec(), object.clone());
            return Ok(None);
        }
        self.forms += 1;
        ensure!(
            self.forms <= MAX_FORM_INVOCATIONS,
            "too many Form invocations to rewrite safely (limit {MAX_FORM_INVOCATIONS})"
        );
        ensure!(
            self.visiting.insert(id),
            "cyclic Form XObject resource at {id:?}"
        );
        let child_resources = resolved_dict(self.doc, source.dict.get(b"Resources").ok())
            .unwrap_or_else(|| resources.clone());
        let child_id = self.doc.new_object_id();
        let built = self.stream(
            &data,
            child_resources,
            Location {
                page: loc.page,
                stream: child_id,
                depth: loc.depth + 1,
                rise,
            },
        )?;
        let mut dict = source.dict;
        for key in [b"Filter".as_slice(), b"DecodeParms", b"Length"] {
            dict.remove(key);
        }
        dict.set("Resources", built.resources);
        self.doc
            .objects
            .insert(child_id, Stream::new(dict, built.bytes).into());
        self.visiting.remove(&id);
        let mut name = format!("PdfTranslateForm{}", child_id.0);
        while original_xobjects.has(name.as_bytes()) || xobjects.has(name.as_bytes()) {
            name.push('_');
        }
        xobjects.set(name.clone(), child_id);
        op.operands = vec![Object::Name(name.into_bytes())];
        Ok(Some(child_id))
    }
}

fn expand_quotes(mut op: Operation) -> Result<smallvec::SmallVec<[Operation; 1]>> {
    let mut ops = smallvec::SmallVec::new();
    if op.operator == "\"" {
        ensure!(op.operands.len() == 3, "invalid double-quote text operator");
        ops.push(Operation::new("Tw", vec![op.operands[0].clone()]));
        ops.push(Operation::new("Tc", vec![op.operands[1].clone()]));
        op.operands = vec![op.operands[2].clone()];
    }
    if matches!(op.operator.as_str(), "'" | "\"") {
        ops.push(Operation::new("T*", vec![]));
        op.operator = "Tj".into();
    }
    ops.push(op);
    Ok(ops)
}

fn append_probe(out: &mut Vec<Operation>, op: &Operation, probe_name: &str, id: usize) {
    let mark = |probe| {
        Operation::new(
            "BDC",
            vec![
                Object::Name(TRACE_TAG.to_vec()),
                dictionary! {"Id" => id as i64, "Probe" => probe}.into(),
            ],
        )
    };
    out.extend([
        mark(true),
        Operation::new("q", vec![]),
        Operation::new(
            "Tf",
            vec![Object::Name(probe_name.as_bytes().to_vec()), 1000.into()],
        ),
        Operation::new("Tc", vec![0.into()]),
        Operation::new("Tw", vec![0.into()]),
        Operation::new("Ts", vec![0.into()]),
        Operation::new("Tz", vec![100.into()]),
        Operation::new("Tj", vec![Object::string_literal(vec![0])]),
        Operation::new("Q", vec![]),
        Operation::new("EMC", vec![]),
        mark(false),
        op.clone(),
        Operation::new("EMC", vec![]),
    ]);
}
