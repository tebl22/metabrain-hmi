//! Spanish wording for the short system messages the core emits in English (alarms, diary, statuses).
//!
//! The core keeps one internal wording; the HMI is bilingual, so the snapshot is translated on the way
//! out when the configured language is Spanish. Unknown text passes through unchanged.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

fn table() -> &'static Vec<(Regex, &'static str)> {
    static T: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    T.get_or_init(|| {
        let rules: &[(&str, &str)] = &[
            (r"^not signed in$", "sin sesión iniciada"),
            (r"^(.+): not signed in$", "$1: sin sesión iniciada"),
            (r"^idle$", "en espera"),
            (r"^no sessions$", "sin sesiones"),
            (r"^standing by$", "listo"),
            (r"^starting$", "iniciando"),
            (r"^idle · (.+)$", "en espera en $1"),
            (r"^awake: reading every brain$", "en marcha: leyendo todos los cerebros"),
            (r"^paused every brain$", "pausaste todos los cerebros"),
            (r"^resumed every brain$", "reanudaste todos los cerebros"),
            (r"^pause(?:d)? (.+)$", "pausaste $1"),
            (r"^resume(?:d)? (.+)$", "reanudaste $1"),
            (r"^(.+) paused \((.+)\)$", "$1 en pausa ($2)"),
            (r"^(.+) paused$", "$1 en pausa"),
            (r"^Paused (.+?): (.+)$", "Pausé $1: $2"),
            (r"^Resumed (.+?): budget available again$", "Reanudé $1: vuelve a haber presupuesto"),
            (r"^Resumed (.+?): no budget data to justify the pause$", "Reanudé $1: no hay datos de presupuesto que justifiquen la pausa"),
            // Quota windows: each variant gets its own rule so the identifier never leaks in Spanish.
            (r"^(.+?): session quota (\d+)%$", "$1: cuota de 5 horas al $2 %"),
            (r"^(.+?): weekly quota (\d+)%$", "$1: cuota semanal al $2 %"),
            (r"^(.+?): weekly_opus quota (\d+)%$", "$1: cuota semanal de Opus al $2 %"),
            (r"^(.+?): weekly_sonnet quota (\d+)%$", "$1: cuota semanal de Sonnet al $2 %"),
            (r"^(.+?): quota (\d+)%$", "$1: cuota al $2 %"),
            (r"^(.+?) saving budget: (.*)$", "$1 ahorrando presupuesto: $2"),
            (r"^(.+?) stepped down to save budget \((.+)\)$", "$1 cambió a un modelo más barato para ahorrar presupuesto ($2)"),
            (r"^(.+?) balance ([\d.]+) (\w+)$", "$1: saldo $2 $3"),
            (r"^(.+?) is down$", "$1 está detenido"),
            (r"^(.+?) was down: restarted it$", "$1 estaba detenido: lo reinicié"),
            (r"^(.+?): no heartbeat for (\d+) min$", "$1: sin señal hace $2 min"),
            (r"^(.+?): quota almost out and no other account has room$", "$1: casi sin cuota y ninguna otra cuenta tiene espacio"),
            (r"^(.+?): back to the main account \(quota reset\)$", "$1: volvió a la cuenta principal (se reinició la cuota)"),
            (r"^configuration saved: (\d+) brains$", "configuración guardada: $1 cerebros"),
            (r"^mode auto for every brain$", "modo automático para todos los cerebros"),
            (r"^mode manual for every brain$", "modo manual para todos los cerebros"),
            (r"^mode paused for every brain$", "modo pausado para todos los cerebros"),
            (r"^mode off for every brain$", "modo apagado para todos los cerebros"),
            (r"^mode (\w+) for every brain$", "modo $1 para todos los cerebros"),
            (r"^launched a subagent: (.+)$", "lanzó un subagente: $1"),
            (r"^stopped process (\d+)$", "detuve el proceso $1"),
            (r"^quota not available \((.+)\)$", "cuota no disponible ($1)"),
            (r"^balance not available \((.+)\)$", "saldo no disponible ($1)"),
            (r"^(\d+) request\(s\) in flight$", "$1 peticiones en curso"),
            (r"^using (.+)$", "usando $1"),
            (r"^delegating to (.+)$", "delegando a $1"),
            (r"^loaded: (.+)$", "cargado: $1"),
            (r"^Budget/quota under pressure: switch to (.+)$", "Presupuesto o cuota al límite: cambia a $1"),
            (r"^Burning faster than usual: effort (.+)$", "Gasta más rápido de lo normal: esfuerzo $1"),
            (r"^Low cache reuse \((\d+)%\).*$", "Poco reuso de caché ($1 %): las sesiones largas reutilizan contexto; reiniciar o cambiar de modelo lo desperdicia"),
            (r"^failed: (.+)$", "falló: $1"),
            (r"^(.+) / (.+) tokens today$", "$1 / $2 tokens hoy"),
        ];
        rules.iter().filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, *r))).collect()
    })
}

/// Translate one short message to Spanish (first matching rule wins).
pub fn es(text: &str) -> String {
    for (re, rep) in table() {
        if re.is_match(text) {
            return re.replace(text, *rep).to_string();
        }
    }
    text.to_string()
}

fn fix_str(v: &mut Value, key: &str) {
    if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
        let t = es(s);
        if t != s {
            v[key] = Value::String(t);
        }
    }
}

/// Translate the human-facing strings of a snapshot in place.
pub fn snapshot_es(snap: &mut Value) {
    for key in ["alarms", "diary"] {
        if let Some(arr) = snap.get_mut(key).and_then(|a| a.as_array_mut()) {
            for a in arr {
                fix_str(a, "text");
            }
        }
    }
    if let Some(brains) = snap.get_mut("brains").and_then(|a| a.as_array_mut()) {
        for b in brains {
            fix_str(b, "status");
            fix_str(b, "warning");
            if let Some(q) = b.get_mut("quota_warning") {
                if let Some(s) = q.as_str() {
                    *q = Value::String(es(s));
                }
            }
            if let Some(eff) = b.get_mut("effective") {
                if let Some(r) = eff.get_mut("reasons").and_then(|x| x.as_array_mut()) {
                    for x in r {
                        if let Some(s) = x.as_str() {
                            *x = Value::String(es(s));
                        }
                    }
                }
            }
            for key in ["pilot", "advice"] {
                if let Some(recs) = b
                    .get_mut(key)
                    .and_then(|a| a.get_mut("recs"))
                    .and_then(|x| x.as_array_mut())
                {
                    for r in recs {
                        fix_str(r, "text");
                    }
                }
            }
            for list in ["sessions", "subagents"] {
                if let Some(arr) = b.get_mut(list).and_then(|x| x.as_array_mut()) {
                    for s in arr {
                        fix_str(s, "status");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translates_known_and_keeps_unknown() {
        assert_eq!(
            es("Claude 1 (Work): not signed in"),
            "Claude 1 (Work): sin sesión iniciada"
        );
        assert_eq!(
            es("Claude 2: session quota 92%"),
            "Claude 2: cuota de 5 horas al 92 %"
        );
        assert_eq!(
            es("awake: reading every brain"),
            "en marcha: leyendo todos los cerebros"
        );
        assert_eq!(es("algo nuevo"), "algo nuevo");
        let mut s = json!({"alarms": [{"text": "Bot is down"}], "brains": [{"status": "idle", "warning": "not signed in"}]});
        snapshot_es(&mut s);
        assert_eq!(s["alarms"][0]["text"], "Bot está detenido");
        assert_eq!(s["brains"][0]["status"], "en espera");
        assert_eq!(s["brains"][0]["warning"], "sin sesión iniciada");
    }

    #[test]
    fn translates_every_quota_window() {
        assert_eq!(
            es("Claude 1 (Work): session quota 92%"),
            "Claude 1 (Work): cuota de 5 horas al 92 %"
        );
        assert_eq!(
            es("Claude 1 (Work): weekly quota 80%"),
            "Claude 1 (Work): cuota semanal al 80 %"
        );
        assert_eq!(
            es("Claude 1 (Work): weekly_opus quota 50%"),
            "Claude 1 (Work): cuota semanal de Opus al 50 %"
        );
        assert_eq!(
            es("Claude 1 (Work): weekly_sonnet quota 5%"),
            "Claude 1 (Work): cuota semanal de Sonnet al 5 %"
        );
        // No English identifier survives in the Spanish wording.
        assert!(!es("C: weekly quota 10%").contains("weekly"));
        assert!(!es("C: session quota 10%").contains("session"));
    }

    #[test]
    fn translates_modes_and_statuses() {
        assert_eq!(
            es("mode auto for every brain"),
            "modo automático para todos los cerebros"
        );
        assert_eq!(
            es("mode turbo for every brain"),
            "modo turbo para todos los cerebros"
        );
    }
}
