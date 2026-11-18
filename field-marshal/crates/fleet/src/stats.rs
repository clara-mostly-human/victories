//! Numbers from the store: records by any column, intervals, and the two tests the release
//! pipeline uses (a sequential test for the arena gate, a paired comparison for the ladder).

use crate::store::Store;
use rusqlite::types::Value as Sql;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Record {
    pub wins: i64,
    pub losses: i64,
    pub draws: i64,
}

impl Record {
    pub fn games(&self) -> i64 {
        self.wins + self.losses + self.draws
    }
    /// Score per game: a win is 1, a draw 1/2.
    pub fn score(&self) -> f64 {
        if self.games() == 0 {
            return 0.0;
        }
        (self.wins as f64 + 0.5 * self.draws as f64) / self.games() as f64
    }
    /// Variance of one game's score.
    pub fn variance(&self) -> f64 {
        let n = self.games().max(1) as f64;
        let m = self.score();
        (self.wins as f64 * (1.0 - m).powi(2) + self.losses as f64 * m.powi(2) + self.draws as f64 * (0.5 - m).powi(2)) / n
    }
    /// 95% Wilson interval of the score.
    pub fn interval(&self) -> (f64, f64) {
        wilson(self.score(), self.games() as f64, 1.96)
    }
    pub fn add(&mut self, result: &str) {
        match result {
            "win" => self.wins += 1,
            "loss" => self.losses += 1,
            _ => self.draws += 1,
        }
    }
}

pub fn wilson(p: f64, n: f64, z: f64) -> (f64, f64) {
    if n <= 0.0 {
        return (0.0, 1.0);
    }
    let d = 1.0 + z * z / n;
    let mid = (p + z * z / (2.0 * n)) / d;
    let half = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / d;
    ((mid - half).max(0.0), (mid + half).min(1.0))
}

/// Standard normal cumulative distribution (Abramowitz and Stegun 7.1.26, error below 1e-7).
pub fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs() / std::f64::consts::SQRT_2);
    let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-(x * x) / 2.0).exp();
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Pass,
    Fail,
    Undecided,
}

/// Sequential test of "score is `s1` or better" against "score is `s0` or worse", with 5%
/// error either way. Returns the log-likelihood ratio and what it says so far.
pub fn sprt(r: &Record, s0: f64, s1: f64) -> (f64, Verdict) {
    let n = r.games() as f64;
    let var = r.variance();
    if n < 2.0 || var <= 0.0 {
        return (0.0, Verdict::Undecided);
    }
    let llr = (s1 - s0) * (2.0 * r.score() - s0 - s1) * n / (2.0 * var);
    let bound = (0.95f64 / 0.05).ln();
    let v = if llr >= bound {
        Verdict::Pass
    } else if llr <= -bound {
        Verdict::Fail
    } else {
        Verdict::Undecided
    };
    (llr, v)
}

/// A filter over the matches table. Only finished, non-voided games count.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub bot: Option<String>,
    pub opponent: Option<String>,
    pub version: Option<String>,
    pub release: Option<String>,
    pub experiment: Option<String>,
    pub since_ms: Option<i64>,
    /// leave out games against our own bots
    pub no_ours: bool,
    /// only opponents rated at least this (games whose opponent rating is unknown are left out)
    pub min_opponent_rating: Option<f64>,
}

impl Filter {
    fn sql(&self) -> (String, Vec<Sql>) {
        let mut w = String::from("result IS NOT NULL AND voided = 0");
        let mut p: Vec<Sql> = Vec::new();
        // `clause` holds one `?`, which becomes the numbered parameter.
        let mut add = |w: &mut String, clause: &str, v: Sql| {
            p.push(v);
            w.push_str(" AND ");
            w.push_str(&clause.replace('?', &format!("?{}", p.len())));
        };
        if let Some(b) = &self.bot {
            add(&mut w, "bot = ?", Sql::Text(b.clone()));
        }
        if let Some(o) = &self.opponent {
            add(&mut w, "opponent = ?", Sql::Text(o.clone()));
        }
        if let Some(v) = &self.version {
            add(&mut w, "version = ?", Sql::Text(v.clone()));
        }
        if let Some(r) = &self.release {
            add(&mut w, "release = ?", Sql::Text(r.clone()));
        }
        if let Some(e) = &self.experiment {
            add(&mut w, "experiment_id = (SELECT id FROM experiments WHERE name = ?)", Sql::Text(e.clone()));
        }
        if let Some(t) = self.since_ms {
            add(&mut w, "started_ms >= ?", Sql::Integer(t));
        }
        if let Some(r) = self.min_opponent_rating {
            add(&mut w, "opponent_rating >= ?", Sql::Real(r));
        }
        if self.no_ours {
            w.push_str(" AND opponent_ours = 0");
        }
        (w, p)
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub record: Record,
    pub rating: f64,
    pub ticks: f64,
    /// mean rating of the opponents, where the ladder's list gave it
    pub opponent_rating: Option<f64>,
}

/// Columns a record can be grouped by. Anything else is refused, so the text never reaches SQL.
pub fn group_column(by: &str) -> Option<&'static str> {
    Some(match by {
        "opponent" => "opponent",
        "version" => "bot || ' ' || COALESCE(version, '?')",
        "release" => "COALESCE(release, '(none)')",
        "playbook" => "COALESCE(playbook, '?')",
        "arm" => "COALESCE(arm, '(no experiment)')",
        "map" => "map",
        "seat" => "'seat ' || COALESCE(seat, '?')",
        "bot" => "bot",
        "cell" => "opponent || ' / ' || map || ' / seat ' || COALESCE(seat, '?')",
        "all" => "'all'",
        _ => return None,
    })
}

impl Store {
    pub fn records(&self, by: &str, f: &Filter) -> rusqlite::Result<Vec<Row>> {
        let col = group_column(by).ok_or_else(|| rusqlite::Error::InvalidParameterName(by.to_string()))?;
        let (w, p) = f.sql();
        let mut q = self.conn.prepare(&format!(
            "SELECT {col} AS k, SUM(result = 'win'), SUM(result = 'loss'), SUM(result NOT IN ('win', 'loss')),
                    COALESCE(SUM(rating_change), 0), COALESCE(AVG(final_tick), 0), AVG(opponent_rating)
             FROM matches WHERE {w} GROUP BY k ORDER BY COUNT(*) DESC, k"
        ))?;
        let rows = q
            .query_map(rusqlite::params_from_iter(p), |r| {
                Ok(Row {
                    key: r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "?".into()),
                    record: Record { wins: r.get(1)?, losses: r.get(2)?, draws: r.get(3)? },
                    rating: r.get(4)?,
                    ticks: r.get(5)?,
                    opponent_rating: r.get(6)?,
                })
            })?
            .collect();
        rows
    }
}

pub fn table(title: &str, rows: &[Row]) -> String {
    let width = rows.iter().map(|r| r.key.chars().count()).max().unwrap_or(4).max(title.chars().count());
    let mut out = format!("{:<width$}  games    W    L    D  score   95% interval  rating  ticks  opp rating\n", title);
    for r in rows {
        let (lo, hi) = r.record.interval();
        out.push_str(&format!(
            "{:<width$}  {:>5} {:>4} {:>4} {:>4}  {:>4.0}%   {:>3.0}% to {:>3.0}%  {:>+6.0}  {:>5.0}  {:>10}\n",
            r.key,
            r.record.games(),
            r.record.wins,
            r.record.losses,
            r.record.draws,
            100.0 * r.record.score(),
            100.0 * lo,
            100.0 * hi,
            r.rating,
            r.ticks,
            r.opponent_rating.map(|x| format!("{x:.0}")).unwrap_or_else(|| "-".into())
        ));
    }
    out
}

/// Candidate against baseline on the ladder, opponent by opponent, so a lucky draw of weak
/// opponents does not count as strength. For every opponent both have met, the difference
/// in score is weighted by how much the pair of samples can tell us.
#[derive(Clone, Debug, Default)]
pub struct Paired {
    pub candidate: Record,
    pub baseline: Record,
    /// opponents both have met, and the games behind them
    pub shared_opponents: usize,
    pub shared_candidate_games: i64,
    pub shared_baseline_games: i64,
    /// weighted mean of (candidate score - baseline score), and its standard error
    pub diff: f64,
    pub se: f64,
}

impl Paired {
    /// Probability that the candidate is the better one, from the normal approximation.
    pub fn confidence(&self) -> f64 {
        if self.se <= 0.0 {
            return 0.5;
        }
        normal_cdf(self.diff / self.se)
    }
}

pub fn paired(candidate: &[Row], baseline: &[Row]) -> Paired {
    let mut out = Paired::default();
    for r in candidate {
        out.candidate.wins += r.record.wins;
        out.candidate.losses += r.record.losses;
        out.candidate.draws += r.record.draws;
    }
    for r in baseline {
        out.baseline.wins += r.record.wins;
        out.baseline.losses += r.record.losses;
        out.baseline.draws += r.record.draws;
    }
    let (mut num, mut den, mut var) = (0.0, 0.0, 0.0);
    for c in candidate {
        let Some(b) = baseline.iter().find(|b| b.key == c.key) else { continue };
        let (nc, nb) = (c.record.games() as f64, b.record.games() as f64);
        if nc == 0.0 || nb == 0.0 {
            continue;
        }
        out.shared_opponents += 1;
        out.shared_candidate_games += c.record.games();
        out.shared_baseline_games += b.record.games();
        // Weight by the harmonic size of the pair. The variance uses a smoothed score so a
        // 3 and 0 record does not claim to be certain.
        let w = nc * nb / (nc + nb);
        let smooth = |r: &Record| (r.wins as f64 + 0.5 * r.draws as f64 + 1.0) / (r.games() as f64 + 2.0);
        let (pc, pb) = (smooth(&c.record), smooth(&b.record));
        num += w * (c.record.score() - b.record.score());
        den += w;
        var += w * w * (pc * (1.0 - pc) / nc + pb * (1.0 - pb) / nb);
    }
    if den > 0.0 {
        out.diff = num / den;
        out.se = var.sqrt() / den;
    }
    out
}

/// Performance rating: the rating at which the expected score against these opponents
/// equals the score actually made. `games` are (opponent rating, score of the game).
/// Returns the rating and its standard error, or `None` without rated games.
pub fn performance(games: &[(f64, f64)]) -> Option<(f64, f64)> {
    if games.is_empty() {
        return None;
    }
    let k = std::f64::consts::LN_10 / 400.0;
    let expect = |r: f64, o: f64| 1.0 / (1.0 + (-(r - o) * k).exp());
    // A clean sweep either way has no finite answer: half a point is added and removed.
    let n = games.len() as f64;
    let made: f64 = games.iter().map(|g| g.1).sum::<f64>().clamp(0.5, n - 0.5);
    let mut r = games.iter().map(|g| g.0).sum::<f64>() / n;
    for _ in 0..60 {
        let e: f64 = games.iter().map(|g| expect(r, g.0)).sum();
        let slope: f64 = games.iter().map(|g| expect(r, g.0) * (1.0 - expect(r, g.0))).sum::<f64>() * k;
        if slope < 1e-12 {
            break;
        }
        let step = ((made - e) / slope).clamp(-200.0, 200.0);
        r += step;
        if step.abs() < 0.01 {
            break;
        }
    }
    let info: f64 = games.iter().map(|g| expect(r, g.0) * (1.0 - expect(r, g.0))).sum::<f64>() * k * k;
    Some((r, 1.0 / info.sqrt()))
}

impl Store {
    /// (opponent rating, score) of every game the filter selects that has a rating.
    pub fn rated_games(&self, f: &Filter) -> rusqlite::Result<Vec<(f64, f64)>> {
        let (w, p) = f.sql();
        let mut q = self.conn.prepare(&format!(
            "SELECT opponent_rating, CASE result WHEN 'win' THEN 1.0 WHEN 'loss' THEN 0.0 ELSE 0.5 END FROM matches WHERE {w} AND opponent_rating IS NOT NULL"
        ))?;
        let rows = q.query_map(rusqlite::params_from_iter(p), |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprt_decides() {
        let strong = Record { wins: 140, losses: 60, draws: 0 };
        assert_eq!(sprt(&strong, 0.5, 0.55).1, Verdict::Pass);
        let weak = Record { wins: 70, losses: 130, draws: 0 };
        assert_eq!(sprt(&weak, 0.5, 0.55).1, Verdict::Fail);
        let even = Record { wins: 21, losses: 19, draws: 0 };
        assert_eq!(sprt(&even, 0.5, 0.55).1, Verdict::Undecided);
    }

    #[test]
    fn performance_rating() {
        // Even score against 2000-rated opponents is a 2000 performance.
        let even: Vec<(f64, f64)> = (0..40).map(|i| (2000.0, (i % 2) as f64)).collect();
        let (r, se) = performance(&even).unwrap();
        assert!((r - 2000.0).abs() < 1.0, "{r}");
        assert!(se > 40.0 && se < 70.0, "{se}");
        // Three wins in four is about 191 points above them.
        let strong: Vec<(f64, f64)> = (0..40).map(|i| (2000.0, if i % 4 == 0 { 0.0 } else { 1.0 })).collect();
        assert!((performance(&strong).unwrap().0 - 2190.8).abs() < 2.0);
        assert!(performance(&[]).is_none());
    }

    #[test]
    fn normal_cdf_values() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((normal_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((normal_cdf(-1.2816) - 0.10).abs() < 1e-4);
    }

    #[test]
    fn paired_ignores_who_was_drawn() {
        let row = |k: &str, w, l| Row { key: k.into(), record: Record { wins: w, losses: l, draws: 0 }, rating: 0.0, ticks: 0.0, opponent_rating: None };
        // Same strength against each opponent, but the candidate met the weak one more often.
        let cand = vec![row("weak", 36, 4), row("strong", 1, 9)];
        let base = vec![row("weak", 9, 1), row("strong", 4, 36)];
        let p = paired(&cand, &base);
        assert!(p.candidate.score() > p.baseline.score() + 0.4);
        assert!(p.diff.abs() < 0.01, "{}", p.diff);
        assert!((p.confidence() - 0.5).abs() < 0.1);
    }
}
