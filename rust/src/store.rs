//! Runtime state (controls the HMI changed), the usage ledger and the event diary (port of store.py).
use crate::util::{data_dir, local_date, midnight, now, read_json, write_json};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub ts: f64,
    pub brain: String,
    pub model: Option<String>,
    pub inp: i64,
    pub out: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub usd: f64,
}

impl Usage {
    pub fn to_json(&self) -> Value {
        json!({"ts": self.ts, "brain": self.brain, "model": self.model, "inp": self.inp, "out": self.out,
               "cache_read": self.cache_read, "cache_write": self.cache_write, "usd": self.usd})
    }
    pub fn from_json(v: &Value) -> Option<Usage> {
        let o = v.as_object()?;
        let n = |k: &str| o.get(k).and_then(|x| x.as_f64());
        // Usage(**d) in Python: every field must be present
        for k in [
            "ts",
            "brain",
            "model",
            "inp",
            "out",
            "cache_read",
            "cache_write",
            "usd",
        ] {
            if !o.contains_key(k) {
                return None;
            }
        }
        Some(Usage {
            ts: n("ts")?,
            brain: o.get("brain")?.as_str()?.to_string(),
            model: o.get("model").and_then(|m| m.as_str()).map(String::from),
            inp: n("inp").unwrap_or(0.0) as i64,
            out: n("out").unwrap_or(0.0) as i64,
            cache_read: n("cache_read").unwrap_or(0.0) as i64,
            cache_write: n("cache_write").unwrap_or(0.0) as i64,
            usd: n("usd").unwrap_or(0.0),
        })
    }
    pub fn total(&self) -> i64 {
        self.inp + self.out + self.cache_write
    }
}

// ---------------------------------------------------------------- State
pub fn state_fields() -> Value {
    json!({"paused": false, "paused_by": null, "model": null, "effort": null, "thinking": null,
           "max_output": null, "downgraded": false, "readonly": false,
           "account": null, "account_since": null, "account_until": null, "account_reason": null})
}

/// Per-brain controls: paused, model, effort, thinking budget, output cap. Survives restarts.
pub struct State {
    pub path: PathBuf,
    pub data: Mutex<Value>,
}

impl State {
    pub fn new(path: Option<PathBuf>) -> State {
        let path = path.unwrap_or_else(|| data_dir().join("state.json"));
        let data = match read_json(&path) {
            Some(v @ Value::Object(_)) => v,
            _ => json!({}),
        };
        State {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn get(&self, bid: &str) -> Value {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = state_fields();
        if let Some(Value::Object(cur)) = d.get(bid) {
            for (k, v) in cur {
                s[k] = v.clone();
            }
        }
        s
    }

    /// Set known fields (unknown keys are ignored, like the Python version).
    pub fn set(&self, bid: &str, kw: Value) -> Value {
        {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            if !d.is_object() {
                *d = json!({});
            }
            let fields = state_fields();
            let cur = d
                .as_object_mut()
                .unwrap()
                .entry(bid.to_string())
                .or_insert_with(|| json!({}));
            if !cur.is_object() {
                *cur = json!({});
            }
            if let Some(kw) = kw.as_object() {
                for (k, v) in kw {
                    if fields.get(k).is_some() {
                        cur[k] = v.clone();
                    }
                }
            }
            let _ = write_json(&self.path, &d);
        }
        self.get(bid)
    }
}

// ---------------------------------------------------------------- Ledger
struct LedgerInner {
    recs: VecDeque<Usage>,
    volatile: HashMap<String, HashMap<String, Usage>>,
}

/// All token/$ usage, by brain and model. Records from the proxy are persisted; adapters
/// that can rebuild their history from their own logs (Claude Code) feed it in memory only.
pub struct Ledger {
    pub path: PathBuf,
    pub keep: f64,
    inner: Mutex<LedgerInner>,
}

impl Ledger {
    pub fn new(path: Option<PathBuf>, keep_days: i64) -> Ledger {
        let path = path.unwrap_or_else(|| data_dir().join("usage.jsonl"));
        let keep = (keep_days.max(1) * 86400) as f64;
        let l = Ledger {
            path,
            keep,
            inner: Mutex::new(LedgerInner {
                recs: VecDeque::new(),
                volatile: HashMap::new(),
            }),
        };
        l.load();
        l
    }

    fn load(&self) {
        let Ok(f) = std::fs::File::open(&self.path) else {
            return;
        };
        let lim = now() - self.keep;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(u) = Usage::from_json(&v) {
                if u.ts >= lim {
                    inner.recs.push_back(u);
                }
            }
        }
    }

    pub fn keep_days(&self) -> usize {
        (self.keep / 86400.0) as usize
    }

    pub fn add(&self, u: Usage, persist: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if persist {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(f, "{}", u.to_json());
            }
        }
        inner.recs.push_back(u);
    }

    pub fn put_volatile(&self, brain: &str, key: &str, u: Usage) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .volatile
            .entry(brain.to_string())
            .or_default()
            .insert(key.to_string(), u);
    }

    /// Replace a brain's whole volatile set (ledger.volatile[brain] = vol).
    pub fn set_volatile(&self, brain: &str, vol: HashMap<String, Usage>) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .volatile
            .insert(brain.to_string(), vol);
    }

    pub fn clear_volatile(&self, brain: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .volatile
            .remove(brain);
    }

    pub fn all(&self) -> Vec<Usage> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let lim = now() - self.keep;
        while inner.recs.front().map(|u| u.ts < lim).unwrap_or(false) {
            inner.recs.pop_front();
        }
        let mut out: Vec<Usage> = inner.recs.iter().filter(|u| u.ts >= lim).cloned().collect();
        for d in inner.volatile.values() {
            out.extend(d.values().filter(|u| u.ts >= lim).cloned());
        }
        out
    }

    /// Totals per brain: today / last 5 h / 7 days, per model today, hourly series (24) and daily history.
    pub fn summary(&self, now_ts: Option<f64>) -> Value {
        let now_ts = now_ts.unwrap_or_else(now);
        let mid = midnight(now_ts);
        let days = self.keep_days();
        struct P {
            today: i64,
            h5: i64,
            d7: i64,
            usd_today: f64,
            usd_d7: f64,
            cache_today: i64,
            models: Vec<(String, i64)>,
            hourly: Vec<i64>,
            daily: Vec<i64>,
            daily_usd: Vec<f64>,
        }
        let mut per: Vec<(String, P)> = vec![];
        let mut mid_cache: HashMap<i64, f64> = HashMap::new();
        for u in self.all() {
            let t = u.total();
            let idx = match per.iter().position(|(b, _)| *b == u.brain) {
                Some(i) => i,
                None => {
                    per.push((
                        u.brain.clone(),
                        P {
                            today: 0,
                            h5: 0,
                            d7: 0,
                            usd_today: 0.0,
                            usd_d7: 0.0,
                            cache_today: 0,
                            models: vec![],
                            hourly: vec![0; 24],
                            daily: vec![0; days],
                            daily_usd: vec![0.0; days],
                        },
                    ));
                    per.len() - 1
                }
            };
            let p = &mut per[idx].1;
            if u.ts >= now_ts - 7.0 * 86400.0 {
                p.d7 += t;
                p.usd_d7 += u.usd;
            }
            if u.ts >= now_ts - 5.0 * 3600.0 {
                p.h5 += t;
            }
            if u.ts >= mid {
                p.today += t;
                p.usd_today += u.usd;
                p.cache_today += u.cache_read;
                let m = u
                    .model
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "?".into());
                match p.models.iter_mut().find(|(k, _)| *k == m) {
                    Some(e) => e.1 += t,
                    None => p.models.push((m, t)),
                }
            }
            let h = ((now_ts - u.ts) / 3600.0).floor() as i64;
            if (0..24).contains(&h) {
                p.hourly[23 - h as usize] += t;
            }
            let hour_key = (u.ts / 3600.0).floor() as i64; // midnight is constant within an hour
            let um = *mid_cache.entry(hour_key).or_insert_with(|| midnight(u.ts));
            let d = ((mid - um) / 86400.0).floor() as i64;
            if d >= 0 && (d as usize) < days {
                p.daily[days - 1 - d as usize] += t;
                p.daily_usd[days - 1 - d as usize] += u.usd;
            }
        }
        let mut out = Map::new();
        for (b, p) in per {
            let mut models = Map::new();
            for (k, v) in p.models {
                models.insert(k, json!(v));
            }
            out.insert(b, json!({"today": p.today, "h5": p.h5, "d7": p.d7, "usd_today": p.usd_today, "usd_d7": p.usd_d7,
                "cache_today": p.cache_today, "models": models, "hourly": p.hourly, "daily": p.daily, "daily_usd": p.daily_usd}));
        }
        Value::Object(out)
    }

    pub fn csv(&self) -> String {
        type CsvKey = (String, String, String);
        type CsvTotals = (i64, i64, i64, i64, f64);
        let mut agg: BTreeMap<CsvKey, CsvTotals> = BTreeMap::new();
        for u in self.all() {
            let k = (
                local_date(u.ts),
                u.brain.clone(),
                u.model.clone().unwrap_or_default(),
            );
            let a = agg.entry(k).or_insert((0, 0, 0, 0, 0.0));
            a.0 += u.inp;
            a.1 += u.out;
            a.2 += u.cache_read;
            a.3 += u.cache_write;
            a.4 += u.usd;
        }
        let mut rows = vec!["date,brain,model,input,output,cache_read,cache_write,usd".to_string()];
        for ((d, b, m), a) in agg {
            rows.push(format!(
                "{d},{b},{m},{},{},{},{},{:.4}",
                a.0, a.1, a.2, a.3, a.4
            ));
        }
        rows.join("\n") + "\n"
    }
}

// ---------------------------------------------------------------- Diary
pub type Listener = Box<dyn Fn(&Value) + Send + Sync>;

pub struct Diary {
    items: Mutex<VecDeque<Value>>,
    maxlen: usize,
    pub listeners: Mutex<Vec<Arc<Listener>>>,
}

impl Diary {
    pub fn new(maxlen: usize) -> Diary {
        Diary {
            items: Mutex::new(VecDeque::new()),
            maxlen,
            listeners: Mutex::new(vec![]),
        }
    }

    pub fn add(&self, who: &str, text: &str, level: &str) -> Value {
        let e = json!({"ts": now(), "who": who, "text": text, "level": level});
        {
            let mut it = self.items.lock().unwrap_or_else(|e| e.into_inner());
            it.push_back(e.clone());
            while it.len() > self.maxlen {
                it.pop_front();
            }
        }
        let ls: Vec<Arc<Listener>> = self
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for f in ls {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&e)));
        }
        e
    }

    pub fn info(&self, who: &str, text: &str) -> Value {
        self.add(who, text, "info")
    }

    pub fn last(&self, n: usize) -> Vec<Value> {
        let it = self.items.lock().unwrap_or_else(|e| e.into_inner());
        let skip = it.len().saturating_sub(n);
        it.iter().skip(skip).cloned().collect()
    }
}
