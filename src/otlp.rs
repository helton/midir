//! A minimal OTLP/HTTP protobuf encoder: ExportTraceServiceRequest and ExportMetricsServiceRequest, exactly the
//! messages and fields Midir exports (opentelemetry-proto v1). Hand-written wire format instead of the
//! opentelemetry crates: a few hundred lines, no code generation, no runtime of its own.

#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Bool(bool),
    Int(i64),
    Double(f64),
}

pub type Attrs = Vec<(String, AttrValue)>;

#[derive(Default)]
pub struct Buf(pub Vec<u8>);

impl Buf {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    pub fn uint(&mut self, field: u32, v: u64) {
        self.key(field, 0);
        self.varint(v);
    }

    pub fn int(&mut self, field: u32, v: i64) {
        self.uint(field, v as u64);
    }

    pub fn bool(&mut self, field: u32, v: bool) {
        self.uint(field, v as u64);
    }

    pub fn fixed64(&mut self, field: u32, v: u64) {
        self.key(field, 1);
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn double(&mut self, field: u32, v: f64) {
        self.fixed64(field, v.to_bits());
    }

    pub fn bytes(&mut self, field: u32, data: &[u8]) {
        self.key(field, 2);
        self.varint(data.len() as u64);
        self.0.extend_from_slice(data);
    }

    pub fn string(&mut self, field: u32, s: &str) {
        self.bytes(field, s.as_bytes());
    }

    pub fn message(&mut self, field: u32, f: impl FnOnce(&mut Buf)) {
        let mut inner = Buf::default();
        f(&mut inner);
        self.bytes(field, &inner.0);
    }

    pub fn packed_fixed64(&mut self, field: u32, values: &[u64]) {
        let mut data = Vec::with_capacity(values.len() * 8);
        for v in values {
            data.extend_from_slice(&v.to_le_bytes());
        }
        self.bytes(field, &data);
    }

    pub fn packed_double(&mut self, field: u32, values: &[f64]) {
        let bits: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
        self.packed_fixed64(field, &bits);
    }
}

/// KeyValue { key = 1; AnyValue value = 2 } with AnyValue { string 1, bool 2, int 3, double 4 }.
pub fn key_values(b: &mut Buf, field: u32, attrs: &Attrs) {
    for (k, v) in attrs {
        b.message(field, |kv| {
            kv.string(1, k);
            kv.message(2, |av| match v {
                AttrValue::Str(s) => av.string(1, s),
                AttrValue::Bool(x) => av.bool(2, *x),
                AttrValue::Int(i) => av.int(3, *i),
                AttrValue::Double(d) => av.double(4, *d),
            });
        });
    }
}

/// SpanKind values (opentelemetry-proto trace.proto).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Internal = 1,
    Server = 2,
    Client = 3,
}

pub struct SpanData {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    pub kind: SpanKind,
    pub name: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub attrs: Attrs,
    pub error: Option<String>,
}

fn scope(b: &mut Buf) {
    b.message(1, |s| s.string(1, "midir"));
}

pub fn encode_traces(resource: &Attrs, spans: &[SpanData]) -> Vec<u8> {
    let mut b = Buf::default();
    b.message(1, |rs| {
        rs.message(1, |r| key_values(r, 1, resource));
        rs.message(2, |ss| {
            scope(ss);
            for s in spans {
                ss.message(2, |sp| {
                    sp.bytes(1, &s.trace_id);
                    sp.bytes(2, &s.span_id);
                    if let Some(parent) = &s.parent_span_id {
                        sp.bytes(4, parent);
                    }
                    sp.string(5, &s.name);
                    sp.uint(6, s.kind as u64);
                    sp.fixed64(7, s.start_ns);
                    sp.fixed64(8, s.end_ns);
                    key_values(sp, 9, &s.attrs);
                    if let Some(msg) = &s.error {
                        sp.message(15, |st| {
                            st.string(2, msg);
                            st.uint(3, 2); // STATUS_CODE_ERROR
                        });
                    }
                });
            }
        });
    });
    b.0
}

/// One cumulative data point of a sum: its attributes, since when it counts, and the value.
pub struct SumPoint {
    pub attrs: Attrs,
    pub start_ns: u64,
    pub value: i64,
}

/// One cumulative data point of a histogram.
pub struct HistogramPoint {
    pub attrs: Attrs,
    pub start_ns: u64,
    pub count: u64,
    pub sum: f64,
    /// one count per bucket, plus the overflow bucket
    pub buckets: Vec<u64>,
    pub min: f64,
    pub max: f64,
}

pub enum Points {
    Sum { monotonic: bool, points: Vec<SumPoint> },
    Histogram { bounds: &'static [f64], points: Vec<HistogramPoint> },
}

pub struct MetricData {
    pub name: &'static str,
    pub description: &'static str,
    pub unit: &'static str,
    pub points: Points,
}

pub fn encode_metrics(resource: &Attrs, metrics: &[MetricData], now_ns: u64) -> Vec<u8> {
    let mut b = Buf::default();
    b.message(1, |rm| {
        rm.message(1, |r| key_values(r, 1, resource));
        rm.message(2, |sm| {
            scope(sm);
            for m in metrics {
                sm.message(2, |mb| {
                    mb.string(1, m.name);
                    mb.string(2, m.description);
                    mb.string(3, m.unit);
                    match &m.points {
                        Points::Sum { monotonic, points } => mb.message(7, |sum| {
                            for p in points {
                                sum.message(1, |dp| {
                                    dp.fixed64(2, p.start_ns);
                                    dp.fixed64(3, now_ns);
                                    dp.fixed64(6, p.value as u64); // as_int (sfixed64)
                                    key_values(dp, 7, &p.attrs);
                                });
                            }
                            sum.uint(2, 2); // AGGREGATION_TEMPORALITY_CUMULATIVE
                            sum.bool(3, *monotonic);
                        }),
                        Points::Histogram { bounds, points } => mb.message(9, |h| {
                            for p in points {
                                h.message(1, |dp| {
                                    dp.fixed64(2, p.start_ns);
                                    dp.fixed64(3, now_ns);
                                    dp.fixed64(4, p.count);
                                    dp.double(5, p.sum);
                                    dp.packed_fixed64(6, &p.buckets);
                                    dp.packed_double(7, bounds);
                                    key_values(dp, 9, &p.attrs);
                                    dp.double(11, p.min);
                                    dp.double(12, p.max);
                                });
                            }
                            h.uint(2, 2);
                        }),
                    }
                });
            }
        });
    });
    b.0
}
