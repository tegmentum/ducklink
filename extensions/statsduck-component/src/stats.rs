//! Pure statistics: the test statistics and p-values, independent of the WIT
//! glue so they can be unit-tested natively against scipy/R reference values.
//! Every entry point returns Option and never panics; invalid input -> None.

use serde_json::{json, Value};
use statrs::distribution::{ChiSquared, ContinuousCDF, FisherSnedecor, Normal, StudentsT};

/// Two-tailed p-value from a Student-t statistic with `df` degrees of freedom.
fn t_two_sided_p(t: f64, df: f64) -> Option<f64> {
    let d = StudentsT::new(0.0, 1.0, df).ok()?;
    // P(|T| >= |t|) = 2 * (1 - CDF(|t|)).
    Some((2.0 * (1.0 - d.cdf(t.abs()))).clamp(0.0, 1.0))
}

/// One-sided / two-sided selection given a signed statistic and its two-sided p.
/// `alt`: "two-sided" | "greater" | "less". Upper-tail prob is for `greater`.
fn directional_p(two_sided: f64, upper_tail: f64, alt: &str) -> f64 {
    match alt {
        "greater" => upper_tail,
        "less" => 1.0 - upper_tail,
        _ => two_sided,
    }
    .clamp(0.0, 1.0)
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample variance (n-1 denominator).
fn var_samp(xs: &[f64], m: f64) -> f64 {
    if xs.len() < 2 {
        return f64::NAN;
    }
    xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() as f64 - 1.0)
}

fn normal_sf(z: f64) -> f64 {
    // Upper-tail of the standard normal: P(Z >= z).
    let n = Normal::new(0.0, 1.0).unwrap();
    1.0 - n.cdf(z)
}

fn normal_two_sided(z: f64) -> f64 {
    (2.0 * normal_sf(z.abs())).clamp(0.0, 1.0)
}

/// One-sample t-test: H0 mean == mu.
pub fn ttest_1samp(x: &[f64], mu: f64, alt: &str) -> Option<Value> {
    if x.len() < 2 {
        return None;
    }
    let n = x.len() as f64;
    let m = mean(x);
    let v = var_samp(x, m);
    let se = (v / n).sqrt();
    if se == 0.0 || !se.is_finite() {
        return None;
    }
    let t = (m - mu) / se;
    let df = n - 1.0;
    let two = t_two_sided_p(t, df)?;
    // upper tail P(T >= t) = 1 - CDF(t).
    let upper = 1.0 - StudentsT::new(0.0, 1.0, df).ok()?.cdf(t);
    let p = directional_p(two, upper, alt);
    let cohens_d = (m - mu) / v.sqrt();
    Some(json!({
        "t_statistic": t, "df": df, "p_value": p,
        "mean_diff": m - mu, "cohens_d": cohens_d, "n": x.len()
    }))
}

/// Two-sample t-test. `equal_var=true` -> pooled (Student); else Welch.
pub fn ttest_2samp(a: &[f64], b: &[f64], equal_var: bool, alt: &str) -> Option<Value> {
    if a.len() < 2 || b.len() < 2 {
        return None;
    }
    let (n1, n2) = (a.len() as f64, b.len() as f64);
    let (m1, m2) = (mean(a), mean(b));
    let (v1, v2) = (var_samp(a, m1), var_samp(b, m2));
    let (t, df) = if equal_var {
        let sp2 = ((n1 - 1.0) * v1 + (n2 - 1.0) * v2) / (n1 + n2 - 2.0);
        let se = (sp2 * (1.0 / n1 + 1.0 / n2)).sqrt();
        ((m1 - m2) / se, n1 + n2 - 2.0)
    } else {
        let se = (v1 / n1 + v2 / n2).sqrt();
        // Welch-Satterthwaite df.
        let df = (v1 / n1 + v2 / n2).powi(2)
            / ((v1 / n1).powi(2) / (n1 - 1.0) + (v2 / n2).powi(2) / (n2 - 1.0));
        ((m1 - m2) / se, df)
    };
    if !t.is_finite() {
        return None;
    }
    let two = t_two_sided_p(t, df)?;
    let upper = 1.0 - StudentsT::new(0.0, 1.0, df).ok()?.cdf(t);
    let p = directional_p(two, upper, alt);
    let sp = (((n1 - 1.0) * v1 + (n2 - 1.0) * v2) / (n1 + n2 - 2.0)).sqrt();
    Some(json!({
        "t_statistic": t, "df": df, "p_value": p,
        "mean_diff": m1 - m2, "cohens_d": (m1 - m2) / sp,
        "n_x": a.len(), "n_y": b.len()
    }))
}

/// Paired t-test (one-sample t on the differences).
pub fn ttest_paired(a: &[f64], b: &[f64], alt: &str) -> Option<Value> {
    if a.len() != b.len() || a.len() < 2 {
        return None;
    }
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    let mut r = ttest_1samp(&d, 0.0, alt)?;
    if let Value::Object(ref mut o) = r {
        o.insert("n".into(), json!(a.len()));
    }
    Some(r)
}

/// Average ranks (1-based) with ties getting the mean rank.
fn rankdata(xs: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..xs.len()).collect();
    idx.sort_by(|&i, &j| xs[i].partial_cmp(&xs[j]).unwrap());
    let mut ranks = vec![0.0; xs.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && xs[idx[j + 1]] == xs[idx[i]] {
            j += 1;
        }
        // positions i..=j are tied; assign the average rank (1-based).
        let avg = ((i + j) as f64) / 2.0 + 1.0;
        for k in i..=j {
            ranks[idx[k]] = avg;
        }
        i = j + 1;
    }
    ranks
}

/// Tie-correction sum for the normal-approximation variance: sum(t^3 - t).
fn tie_correction(xs: &[f64]) -> f64 {
    let mut sorted = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut total = 0.0;
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[i] {
            j += 1;
        }
        let t = (j - i + 1) as f64;
        total += t.powi(3) - t;
        i = j + 1;
    }
    total
}

/// Mann-Whitney U test (normal approximation with tie + optional continuity).
pub fn mann_whitney_u(a: &[f64], b: &[f64], alt: &str, continuity: bool) -> Option<Value> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let (n1, n2) = (a.len() as f64, b.len() as f64);
    let mut all = a.to_vec();
    all.extend_from_slice(b);
    let ranks = rankdata(&all);
    let r1: f64 = ranks[..a.len()].iter().sum();
    let u1 = r1 - n1 * (n1 + 1.0) / 2.0;
    let u2 = n1 * n2 - u1;
    let u = u1.min(u2);
    let mu = n1 * n2 / 2.0;
    let n = n1 + n2;
    let tie = tie_correction(&all);
    let sigma2 = (n1 * n2 / 12.0) * ((n + 1.0) - tie / (n * (n - 1.0)));
    if sigma2 <= 0.0 {
        return None;
    }
    let sigma = sigma2.sqrt();
    let cc = if continuity { 0.5 } else { 0.0 };
    // z on U1 (so direction matches a > b => U1 large).
    let z = ((u1 - mu).abs() - cc).max(0.0) / sigma * (u1 - mu).signum();
    let two = normal_two_sided(z);
    let upper = normal_sf(z);
    let p = directional_p(two, upper, alt);
    let rank_biserial = 1.0 - 2.0 * u1 / (n1 * n2);
    Some(json!({
        "u_statistic": u, "z_statistic": z, "p_value": p,
        "rank_biserial": rank_biserial, "n_x": a.len(), "n_y": b.len()
    }))
}

/// Wilcoxon signed-rank test (normal approximation, drops zero differences).
pub fn wilcoxon_signed_rank(a: &[f64], b: &[f64], alt: &str, continuity: bool) -> Option<Value> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let diffs: Vec<f64> = a
        .iter()
        .zip(b)
        .map(|(x, y)| x - y)
        .filter(|d| *d != 0.0)
        .collect();
    if diffs.len() < 2 {
        return None;
    }
    let abs: Vec<f64> = diffs.iter().map(|d| d.abs()).collect();
    let ranks = rankdata(&abs);
    let mut w_plus = 0.0;
    let mut w_minus = 0.0;
    for (d, r) in diffs.iter().zip(&ranks) {
        if *d > 0.0 {
            w_plus += r;
        } else {
            w_minus += r;
        }
    }
    let w = w_plus.min(w_minus);
    let n = diffs.len() as f64;
    let mu = n * (n + 1.0) / 4.0;
    let tie = tie_correction(&abs);
    let sigma2 = n * (n + 1.0) * (2.0 * n + 1.0) / 24.0 - tie / 48.0;
    if sigma2 <= 0.0 {
        return None;
    }
    let sigma = sigma2.sqrt();
    let cc = if continuity { 0.5 } else { 0.0 };
    let z = ((w_plus - mu).abs() - cc).max(0.0) / sigma * (w_plus - mu).signum();
    let two = normal_two_sided(z);
    let upper = normal_sf(z);
    let p = directional_p(two, upper, alt);
    let effect_r = z.abs() / n.sqrt();
    Some(json!({
        "w_statistic": w, "z_statistic": z, "p_value": p,
        "effect_size_r": effect_r, "n": diffs.len()
    }))
}

/// Sign test for one sample vs `mu` (exact binomial, two-sided/one-sided).
pub fn sign_test_1samp(x: &[f64], mu: f64, alt: &str) -> Option<Value> {
    if x.is_empty() {
        return None;
    }
    let n_pos = x.iter().filter(|&&v| v > mu).count();
    let n_neg = x.iter().filter(|&&v| v < mu).count();
    let n_zero = x.len() - n_pos - n_neg;
    let n = n_pos + n_neg;
    if n == 0 {
        return None;
    }
    let p = binom_test_p(n_pos, n, alt);
    Some(json!({
        "m_statistic": n_pos as i64 - n_neg as i64,
        "n_pos": n_pos, "n_neg": n_neg, "n_zero": n_zero, "p_value": p
    }))
}

/// Paired sign test (sign test on the differences vs 0).
pub fn sign_test_paired(a: &[f64], b: &[f64], alt: &str) -> Option<Value> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    sign_test_1samp(&d, 0.0, alt)
}

/// Exact two-sided/one-sided binomial test p-value for k successes of n, p=0.5.
fn binom_test_p(k: usize, n: usize, alt: &str) -> f64 {
    let pmf = |i: usize| -> f64 {
        // C(n,i) * 0.5^n via log-gamma to stay stable.
        let log_c = ln_gamma((n + 1) as f64)
            - ln_gamma((i + 1) as f64)
            - ln_gamma((n - i + 1) as f64);
        (log_c + (n as f64) * (0.5f64).ln()).exp()
    };
    let cdf_le = |k: usize| (0..=k).map(pmf).sum::<f64>();
    let sf_ge = |k: usize| (k..=n).map(pmf).sum::<f64>();
    match alt {
        "greater" => sf_ge(k).clamp(0.0, 1.0),
        "less" => cdf_le(k).clamp(0.0, 1.0),
        _ => {
            // Two-sided: sum of all outcomes no more probable than observed.
            let p_obs = pmf(k);
            (0..=n)
                .map(pmf)
                .filter(|&p| p <= p_obs * (1.0 + 1e-7))
                .sum::<f64>()
                .clamp(0.0, 1.0)
        }
    }
}

fn ln_gamma(x: f64) -> f64 {
    statrs::function::gamma::ln_gamma(x)
}

/// Pearson correlation test (r, t-statistic, two/one-sided p, n).
pub fn pearson_test(x: &[f64], y: &[f64], alt: &str) -> Option<Value> {
    if x.len() != y.len() || x.len() < 3 {
        return None;
    }
    let n = x.len() as f64;
    let (mx, my) = (mean(x), mean(y));
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx).powi(2);
        syy += (b - my).powi(2);
    }
    if sxx == 0.0 || syy == 0.0 {
        return None;
    }
    let r = (sxy / (sxx * syy).sqrt()).clamp(-1.0, 1.0);
    let df = n - 2.0;
    let t = r * (df / (1.0 - r * r)).sqrt();
    let two = t_two_sided_p(t, df)?;
    let upper = 1.0 - StudentsT::new(0.0, 1.0, df).ok()?.cdf(t);
    let p = directional_p(two, upper, alt);
    Some(json!({
        "r": r, "t_statistic": t, "df": df, "p_value": p, "n": x.len()
    }))
}

/// Spearman rank correlation test (Pearson on the ranks).
pub fn spearman_test(x: &[f64], y: &[f64], alt: &str) -> Option<Value> {
    if x.len() != y.len() || x.len() < 3 {
        return None;
    }
    let rx = rankdata(x);
    let ry = rankdata(y);
    let mut r = pearson_test(&rx, &ry, alt)?;
    if let Value::Object(ref mut o) = r {
        // rename r -> rho to match the-stats-duck's surface.
        if let Some(v) = o.remove("r") {
            o.insert("rho".into(), v);
        }
    }
    Some(r)
}

/// One-way ANOVA over `groups` (slice of group samples).
pub fn anova_oneway(groups: &[Vec<f64>]) -> Option<Value> {
    let groups: Vec<&Vec<f64>> = groups.iter().filter(|g| !g.is_empty()).collect();
    if groups.len() < 2 {
        return None;
    }
    let n: f64 = groups.iter().map(|g| g.len() as f64).sum();
    let grand: f64 = groups.iter().flat_map(|g| g.iter()).sum::<f64>() / n;
    let mut ss_between = 0.0;
    let mut ss_within = 0.0;
    for g in &groups {
        let gm = mean(g);
        ss_between += g.len() as f64 * (gm - grand).powi(2);
        for &v in g.iter() {
            ss_within += (v - gm).powi(2);
        }
    }
    let k = groups.len() as f64;
    let df_b = k - 1.0;
    let df_w = n - k;
    if df_w <= 0.0 || ss_within <= 0.0 {
        return None;
    }
    let ms_b = ss_between / df_b;
    let ms_w = ss_within / df_w;
    let f = ms_b / ms_w;
    let d = FisherSnedecor::new(df_b, df_w).ok()?;
    let p = (1.0 - d.cdf(f)).clamp(0.0, 1.0);
    let eta2 = ss_between / (ss_between + ss_within);
    Some(json!({
        "f_statistic": f, "df_between": df_b, "df_within": df_w,
        "p_value": p, "ss_between": ss_between, "ss_within": ss_within,
        "eta_squared": eta2, "n_groups": groups.len(), "n": n as i64
    }))
}

/// Chi-square goodness-of-fit. `observed` counts; optional `expected` (else uniform).
pub fn chisq_goodness_of_fit(observed: &[f64], expected: Option<&[f64]>) -> Option<Value> {
    if observed.len() < 2 {
        return None;
    }
    let total: f64 = observed.iter().sum();
    let exp: Vec<f64> = match expected {
        Some(e) if e.len() == observed.len() => {
            let es: f64 = e.iter().sum();
            // scale provided expected to the observed total.
            e.iter().map(|v| v * total / es).collect()
        }
        _ => vec![total / observed.len() as f64; observed.len()],
    };
    if exp.iter().any(|&e| e <= 0.0) {
        return None;
    }
    let chi2: f64 = observed
        .iter()
        .zip(&exp)
        .map(|(o, e)| (o - e).powi(2) / e)
        .sum();
    let df = observed.len() as f64 - 1.0;
    let d = ChiSquared::new(df).ok()?;
    let p = (1.0 - d.cdf(chi2)).clamp(0.0, 1.0);
    Some(json!({
        "chi_square": chi2, "df": df, "p_value": p,
        "n": total, "n_categories": observed.len()
    }))
}

/// Chi-square test of independence on a contingency table (rows of counts).
pub fn chisq_independence(table: &[Vec<f64>], continuity: bool) -> Option<Value> {
    let n_rows = table.len();
    if n_rows < 2 {
        return None;
    }
    let n_cols = table[0].len();
    if n_cols < 2 || table.iter().any(|r| r.len() != n_cols) {
        return None;
    }
    let row_tot: Vec<f64> = table.iter().map(|r| r.iter().sum()).collect();
    let mut col_tot = vec![0.0; n_cols];
    for r in table {
        for (j, &v) in r.iter().enumerate() {
            col_tot[j] += v;
        }
    }
    let total: f64 = row_tot.iter().sum();
    if total == 0.0 {
        return None;
    }
    let df = ((n_rows - 1) * (n_cols - 1)) as f64;
    // Yates' continuity correction only applies to a 2x2 table.
    let yates = continuity && n_rows == 2 && n_cols == 2;
    let mut chi2 = 0.0;
    for i in 0..n_rows {
        for j in 0..n_cols {
            let e = row_tot[i] * col_tot[j] / total;
            if e <= 0.0 {
                return None;
            }
            let mut diff = (table[i][j] - e).abs();
            if yates {
                diff = (diff - 0.5).max(0.0);
            }
            chi2 += diff * diff / e;
        }
    }
    let d = ChiSquared::new(df).ok()?;
    let p = (1.0 - d.cdf(chi2)).clamp(0.0, 1.0);
    Some(json!({
        "chi_square": chi2, "df": df, "p_value": p,
        "n": total, "n_rows": n_rows, "n_cols": n_cols
    }))
}

/// Jarque-Bera normality test.
pub fn jarque_bera(x: &[f64]) -> Option<Value> {
    if x.len() < 4 {
        return None;
    }
    let n = x.len() as f64;
    let m = mean(x);
    let m2 = x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / n;
    let m3 = x.iter().map(|v| (v - m).powi(3)).sum::<f64>() / n;
    let m4 = x.iter().map(|v| (v - m).powi(4)).sum::<f64>() / n;
    if m2 <= 0.0 {
        return None;
    }
    let skew = m3 / m2.powf(1.5);
    let exkurt = m4 / (m2 * m2) - 3.0;
    let jb = n / 6.0 * (skew * skew + exkurt * exkurt / 4.0);
    let d = ChiSquared::new(2.0).ok()?;
    let p = (1.0 - d.cdf(jb)).clamp(0.0, 1.0);
    Some(json!({
        "jb_statistic": jb, "skewness": skew, "excess_kurtosis": exkurt,
        "df": 2, "p_value": p, "n": x.len()
    }))
}

/// Two-sample Kolmogorov-Smirnov test (asymptotic p-value).
pub fn ks_test_2samp(a: &[f64], b: &[f64]) -> Option<Value> {
    if a.len() < 2 || b.len() < 2 {
        return None;
    }
    let mut xs = a.to_vec();
    xs.sort_by(|p, q| p.partial_cmp(q).unwrap());
    let mut ys = b.to_vec();
    ys.sort_by(|p, q| p.partial_cmp(q).unwrap());
    let (n1, n2) = (a.len() as f64, b.len() as f64);
    // Step through the merged set, tracking the two empirical CDFs.
    let mut all: Vec<f64> = a.iter().chain(b.iter()).cloned().collect();
    all.sort_by(|p, q| p.partial_cmp(q).unwrap());
    all.dedup();
    let cdf = |sorted: &[f64], v: f64, n: f64| {
        sorted.iter().filter(|&&s| s <= v).count() as f64 / n
    };
    let mut d: f64 = 0.0;
    for &v in &all {
        d = d.max((cdf(&xs, v, n1) - cdf(&ys, v, n2)).abs());
    }
    let en = (n1 * n2 / (n1 + n2)).sqrt();
    let p = ks_pvalue((en + 0.12 + 0.11 / en) * d);
    Some(json!({
        "d_statistic": d, "p_value": p, "n_x": a.len(), "n_y": b.len()
    }))
}

/// Kolmogorov distribution survival function Q(lambda) (the KS asymptotic p).
fn ks_pvalue(lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return 1.0;
    }
    let mut sum = 0.0;
    for j in 1..=100 {
        let term = 2.0 * (-1f64).powi(j - 1) * (-2.0 * (j as f64).powi(2) * lambda * lambda).exp();
        sum += term;
        if term.abs() < 1e-12 {
            break;
        }
    }
    sum.clamp(0.0, 1.0)
}

/// Multiple-testing p-value adjustment. methods: bonferroni, holm, hochberg,
/// BH/fdr, BY, none. Returns the adjusted p-values in the input order.
pub fn adjust_p(p: &[f64], method: &str) -> Option<Vec<f64>> {
    if p.is_empty() || p.iter().any(|v| !(0.0..=1.0).contains(v)) {
        return None;
    }
    let m = p.len();
    let mf = m as f64;
    let clamp1 = |v: f64| v.min(1.0);
    let out = match method.to_ascii_lowercase().as_str() {
        "none" => p.to_vec(),
        "bonferroni" => p.iter().map(|&v| clamp1(v * mf)).collect(),
        "holm" => {
            // ascending order; cumulative max of (m - i) * p.
            let mut order: Vec<usize> = (0..m).collect();
            order.sort_by(|&i, &j| p[i].partial_cmp(&p[j]).unwrap());
            let mut adj = vec![0.0; m];
            let mut running: f64 = 0.0;
            for (rank, &idx) in order.iter().enumerate() {
                running = running.max((mf - rank as f64) * p[idx]);
                adj[idx] = clamp1(running);
            }
            adj
        }
        "hochberg" => {
            // descending order; cumulative min of (m - i) * p.
            let mut order: Vec<usize> = (0..m).collect();
            order.sort_by(|&i, &j| p[j].partial_cmp(&p[i]).unwrap());
            let mut adj = vec![0.0; m];
            let mut running = f64::INFINITY;
            for (k, &idx) in order.iter().enumerate() {
                let rank = m - k; // 1-based rank from largest
                running = running.min((mf - rank as f64 + 1.0) * p[idx]);
                adj[idx] = clamp1(running);
            }
            adj
        }
        "bh" | "fdr" => {
            let mut order: Vec<usize> = (0..m).collect();
            order.sort_by(|&i, &j| p[j].partial_cmp(&p[i]).unwrap()); // descending
            let mut adj = vec![0.0; m];
            let mut running = f64::INFINITY;
            for (k, &idx) in order.iter().enumerate() {
                let rank = m - k; // 1-based ascending rank
                running = running.min(mf / rank as f64 * p[idx]);
                adj[idx] = clamp1(running);
            }
            adj
        }
        "by" => {
            let cm: f64 = (1..=m).map(|i| 1.0 / i as f64).sum();
            let mut order: Vec<usize> = (0..m).collect();
            order.sort_by(|&i, &j| p[j].partial_cmp(&p[i]).unwrap());
            let mut adj = vec![0.0; m];
            let mut running = f64::INFINITY;
            for (k, &idx) in order.iter().enumerate() {
                let rank = m - k;
                running = running.min(cm * mf / rank as f64 * p[idx]);
                adj[idx] = clamp1(running);
            }
            adj
        }
        _ => return None,
    };
    Some(out)
}

// ============================================================================
// PRIORITY 1 -- the "hard" normality / GOF / correlation tests with real
// algorithms and correct p-values, matched to scipy reference values.
// ============================================================================

/// Inverse standard-normal CDF (Acklam's rational approximation, ~1e-9).
fn norm_ppf(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    // Coefficients for Acklam's algorithm.
    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.383577518672690e+02,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];
    let plow = 0.02425;
    let phigh = 1.0 - plow;
    if p < plow {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= phigh {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

fn norm_cdf(x: f64) -> f64 {
    Normal::new(0.0, 1.0).unwrap().cdf(x)
}

/// Shapiro-Wilk normality test via the Royston (1992) AS R94 algorithm.
/// Returns {W, p_value, n}. Valid for 3 <= n <= 5000.
pub fn shapiro_wilk(x: &[f64]) -> Option<Value> {
    let n = x.len();
    if n < 3 || n > 5000 {
        return None;
    }
    let nf = n as f64;
    let mut xs = x.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());

    // m_i = Phi^-1((i - 0.375) / (n + 0.25)), i = 1..n.
    let mut m = vec![0.0_f64; n];
    for i in 0..n {
        m[i] = norm_ppf(((i + 1) as f64 - 0.375) / (nf + 0.25));
    }
    let ssm: f64 = m.iter().map(|v| v * v).sum();
    let rsn = 1.0 / nf.sqrt();

    // Royston's polynomial corrections for the two largest weights.
    // Ascending-order polynomial: c[0] is the constant term, c[k] multiplies u^k.
    let poly = |c: &[f64], u: f64| -> f64 {
        let mut s = c[0];
        let mut uu = 1.0;
        for &coef in &c[1..] {
            uu *= u;
            s += coef * uu;
        }
        s
    };
    // Royston AS R94 weight-correction coefficients (ascending in u = 1/sqrt(n)).
    let c1 = [0.0, 0.221157, -0.147981, -2.071190, 4.434685, -2.706056];
    let c2 = [0.0, 0.042981, -0.293762, -1.752461, 5.682633, -3.582633];
    let sqm = ssm.sqrt();
    let mut a = vec![0.0_f64; n];
    let an1 = m[n - 1] / sqm + poly(&c1, rsn);
    let an2 = m[n - 2] / sqm + poly(&c2, rsn);

    // `m` is antisymmetric (m[i] == -m[n-1-i]). Following AS R94: the two
    // largest weights get the polynomial-corrected an1/an2; the interior
    // weights are m_i / sqrt(phi) where phi normalizes sum(a_i^2) to 1.
    // The weights `a` are themselves antisymmetric: a[0]=-an1, a[n-1]=+an1.
    let (i1, phi);
    if n > 5 {
        i1 = 2; // number of corrected weights at each end
        phi = (ssm - 2.0 * m[n - 1] * m[n - 1] - 2.0 * m[n - 2] * m[n - 2])
            / (1.0 - 2.0 * an1 * an1 - 2.0 * an2 * an2);
    } else {
        i1 = 1;
        phi = (ssm - 2.0 * m[n - 1] * m[n - 1]) / (1.0 - 2.0 * an1 * an1);
    }
    let sqphi = phi.sqrt();
    for i in i1..(n - i1) {
        a[i] = m[i] / sqphi;
    }
    a[0] = -an1;
    a[n - 1] = an1;
    if n > 5 {
        a[1] = -an2;
        a[n - 2] = an2;
    }

    // W = (sum a_i * x_(i))^2 / sum (x_i - xbar)^2.
    let mean_x = xs.iter().sum::<f64>() / nf;
    let ssx: f64 = xs.iter().map(|v| (v - mean_x).powi(2)).sum();
    if ssx <= 0.0 {
        return None;
    }
    let b: f64 = a.iter().zip(&xs).map(|(ai, xi)| ai * xi).sum();
    let w = (b * b) / ssx;
    let w = w.min(1.0);

    // Royston's normalizing transformation for the p-value.
    let p_value = if n == 3 {
        // exact small-sample (Royston): pi/6 * (asin(sqrt(W)) - asin(sqrt(3/4)))
        let pw = (std::f64::consts::PI / 6.0)
            * ((w.sqrt()).asin() - (0.75_f64.sqrt()).asin());
        (1.0 - pw).clamp(0.0, 1.0)
    } else if n <= 11 {
        // small-sample branch: gamma transform of (1-W), then standardize.
        let g = [-2.273, 0.459];
        let gamma = g[0] + g[1] * nf;
        let c3 = [0.5440, -0.39978, 0.025054, -6.714e-4];
        let c4 = [1.3822, -0.77857, 0.062767, -0.0020322];
        let mu = poly(&c3, nf);
        let sigma = poly(&c4, nf).exp();
        let y = -(gamma - (1.0 - w).ln()).ln();
        let z = (y - mu) / sigma;
        (1.0 - norm_cdf(z)).clamp(0.0, 1.0)
    } else {
        // large-sample branch: log(1-W) standardized via log(n) polynomials.
        let c5 = [-1.5861, -0.31082, -0.083751, 0.0038915];
        let c6 = [-0.4803, -0.082676, 0.0030302];
        let ln_n = nf.ln();
        let mu = poly(&c5, ln_n);
        let sigma = poly(&c6, ln_n).exp();
        let y = (1.0 - w).ln();
        let z = (y - mu) / sigma;
        (1.0 - norm_cdf(z)).clamp(0.0, 1.0)
    };
    Some(json!({ "W": w, "p_value": p_value, "n": n }))
}

/// Anderson-Darling test for normality (mean+var estimated from the sample).
/// Returns {A_squared, A_star, p_value, n}. p-value via D'Agostino & Stephens.
pub fn anderson_darling(x: &[f64]) -> Option<Value> {
    let n = x.len();
    if n < 8 {
        return None;
    }
    let nf = n as f64;
    let mut xs = x.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_x = xs.iter().sum::<f64>() / nf;
    let var = xs.iter().map(|v| (v - mean_x).powi(2)).sum::<f64>() / (nf - 1.0);
    let sd = var.sqrt();
    if sd <= 0.0 {
        return None;
    }
    let mut s = 0.0;
    for i in 0..n {
        let zi = norm_cdf((xs[i] - mean_x) / sd);
        let zj = norm_cdf((xs[n - 1 - i] - mean_x) / sd);
        // guard log domain
        let zi = zi.clamp(1e-12, 1.0 - 1e-12);
        let zj = zj.clamp(1e-12, 1.0 - 1e-12);
        s += (2.0 * (i + 1) as f64 - 1.0) * (zi.ln() + (1.0 - zj).ln());
    }
    let a2 = -nf - s / nf;
    let a_star = a2 * (1.0 + 0.75 / nf + 2.25 / (nf * nf));
    // D'Agostino & Stephens (1986) piecewise p-value for the case where mu and
    // sigma are estimated (uses A_star).
    let p = if a_star < 0.2 {
        1.0 - (-13.436 + 101.14 * a_star - 223.73 * a_star * a_star).exp()
    } else if a_star < 0.34 {
        1.0 - (-8.318 + 42.796 * a_star - 59.938 * a_star * a_star).exp()
    } else if a_star < 0.6 {
        (1.2937 - 5.709 * a_star + 0.0186 * a_star * a_star).exp()
    } else if a_star < 10.0 {
        (1.0776 - 2.30695 * a_star + 0.43424 * a_star * a_star
            - 0.082433 * a_star.powi(3)
            + 0.0085481 * a_star.powi(4)
            - 0.00034745 * a_star.powi(5))
        .exp()
    } else {
        0.0
    };
    Some(json!({
        "A_squared": a2, "A_star": a_star,
        "p_value": p.clamp(0.0, 1.0), "n": n
    }))
}

/// One-sample Kolmogorov-Smirnov test against a named distribution.
/// dist: "normal" {mean,std} | "uniform" {min,max} | "exponential" {rate}.
/// Returns {D, p_value, n}. p-value via the asymptotic Kolmogorov series.
pub fn ks_test_1samp(sample: &[f64], dist: &str, params: &Value) -> Option<Value> {
    let n = sample.len();
    if n < 1 {
        return None;
    }
    let p = |k: &str, d: f64| params.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
    let cdf: Box<dyn Fn(f64) -> f64> = match dist.to_ascii_lowercase().as_str() {
        "normal" | "norm" | "gaussian" => {
            let mean = p("mean", 0.0);
            let std = p("std", 1.0);
            if std <= 0.0 {
                return None;
            }
            Box::new(move |v: f64| norm_cdf((v - mean) / std))
        }
        "uniform" | "unif" => {
            let lo = p("min", 0.0);
            let hi = p("max", 1.0);
            if hi <= lo {
                return None;
            }
            Box::new(move |v: f64| ((v - lo) / (hi - lo)).clamp(0.0, 1.0))
        }
        "exponential" | "expon" | "exp" => {
            let rate = p("rate", 1.0);
            if rate <= 0.0 {
                return None;
            }
            Box::new(move |v: f64| if v < 0.0 { 0.0 } else { 1.0 - (-rate * v).exp() })
        }
        _ => return None,
    };
    let mut xs = sample.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let nf = n as f64;
    let mut d: f64 = 0.0;
    for (i, &v) in xs.iter().enumerate() {
        let f = cdf(v);
        let d_plus = (i + 1) as f64 / nf - f;
        let d_minus = f - i as f64 / nf;
        d = d.max(d_plus).max(d_minus);
    }
    let en = nf.sqrt();
    let p_value = ks_pvalue((en + 0.12 + 0.11 / en) * d);
    Some(json!({ "D": d, "p_value": p_value, "n": n }))
}

/// Kendall's tau-b correlation with a normal-approximation (two-sided) p-value.
/// Returns {tau, p_value, z_statistic, n, concordant, discordant}.
pub fn kendall_test(x: &[f64], y: &[f64]) -> Option<Value> {
    if x.len() != y.len() || x.len() < 3 {
        return None;
    }
    let n = x.len();
    let nf = n as f64;
    let mut concordant = 0_i64;
    let mut discordant = 0_i64;
    for i in 0..n {
        for j in (i + 1)..n {
            let a = (x[i] - x[j]).partial_cmp(&0.0).unwrap();
            let b = (y[i] - y[j]).partial_cmp(&0.0).unwrap();
            let dx = x[i] != x[j];
            let dy = y[i] != y[j];
            if dx && dy {
                if a == b {
                    concordant += 1;
                } else {
                    discordant += 1;
                }
            }
        }
    }
    let n0 = nf * (nf - 1.0) / 2.0;
    // tie corrections
    let tie_sum = |v: &[f64]| -> (f64, f64, f64) {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let (mut t1, mut t2, mut t3) = (0.0, 0.0, 0.0);
        let mut i = 0;
        while i < s.len() {
            let mut j = i;
            while j + 1 < s.len() && s[j + 1] == s[i] {
                j += 1;
            }
            let t = (j - i + 1) as f64;
            t1 += t * (t - 1.0) / 2.0;
            t2 += t * (t - 1.0) * (t - 2.0);
            t3 += t * (t - 1.0) * (2.0 * t + 5.0);
            i = j + 1;
        }
        (t1, t2, t3)
    };
    let (n1, n1b, v1t) = tie_sum(x);
    let (n2, n2b, v2t) = tie_sum(y);
    let denom = ((n0 - n1) * (n0 - n2)).sqrt();
    if denom <= 0.0 {
        return None;
    }
    let tau = (concordant - discordant) as f64 / denom;
    // Variance with tie correction (scipy's formula).
    let v0 = nf * (nf - 1.0) * (2.0 * nf + 5.0);
    let var = (v0 - v1t - v2t) / 18.0
        + (n1b * n2b) / (9.0 * nf * (nf - 1.0) * (nf - 2.0))
        + (n1 * n2) / (2.0 * n0);
    if var <= 0.0 {
        return None;
    }
    let s = (concordant - discordant) as f64;
    let z = s / var.sqrt();
    let p_value = normal_two_sided(z);
    Some(json!({
        "tau": tau, "p_value": p_value, "z_statistic": z,
        "n": n, "concordant": concordant, "discordant": discordant
    }))
}

/// Poisson-binomial CDF P(X <= k) for independent Bernoulli(p_i) via the exact
/// DP recurrence. Out-of-range probabilities or k -> None.
pub fn poibin_cdf(probs: &[f64], k: i64) -> Option<f64> {
    if probs.is_empty() || probs.iter().any(|p| !(0.0..=1.0).contains(p)) {
        return None;
    }
    let m = probs.len();
    if k < 0 {
        return Some(0.0);
    }
    if k as usize >= m {
        return Some(1.0);
    }
    // dp[j] = P(exactly j successes among processed trials).
    let mut dp = vec![0.0_f64; m + 1];
    dp[0] = 1.0;
    for &p in probs {
        let q = 1.0 - p;
        for j in (1..=m).rev() {
            dp[j] = dp[j] * q + dp[j - 1] * p;
        }
        dp[0] *= q;
    }
    let cdf: f64 = dp[..=(k as usize)].iter().sum();
    Some(cdf.clamp(0.0, 1.0))
}

// ============================================================================
// PRIORITY 2 -- table-function backends (OLS, correlation matrix, descriptive
// summary, histogram binning). These return plain Rust structures; the WIT glue
// in lib.rs turns them into resultset rows.
// ============================================================================

/// Result of an OLS fit, enough to build both lm() and lm_summary() tables.
pub struct OlsFit {
    pub terms: Vec<String>,
    pub estimate: Vec<f64>,
    pub std_error: Vec<f64>,
    pub t_value: Vec<f64>,
    pub p_value: Vec<f64>,
    pub r_squared: f64,
    pub adj_r_squared: f64,
    pub f_statistic: f64,
    pub f_pvalue: f64,
    pub residual_std_error: f64,
    pub df_resid: f64,
}

/// Solve A x = b for a small square system via Gauss-Jordan elimination with
/// partial pivoting. Also returns the inverse of A (for the covariance matrix).
/// Returns None if A is singular.
fn gauss_jordan_inverse(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    // augmented [A | I]
    let mut m: Vec<Vec<f64>> = (0..n)
        .map(|i| {
            let mut row = a[i].clone();
            for j in 0..n {
                row.push(if i == j { 1.0 } else { 0.0 });
            }
            row
        })
        .collect();
    for col in 0..n {
        // partial pivot
        let mut piv = col;
        for r in (col + 1)..n {
            if m[r][col].abs() > m[piv][col].abs() {
                piv = r;
            }
        }
        if m[piv][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, piv);
        let d = m[col][col];
        for c in 0..2 * n {
            m[col][c] /= d;
        }
        for r in 0..n {
            if r != col {
                let f = m[r][col];
                if f != 0.0 {
                    for c in 0..2 * n {
                        m[r][c] -= f * m[col][c];
                    }
                }
            }
        }
    }
    Some(m.iter().map(|row| row[n..].to_vec()).collect())
}

/// OLS fit of y on the predictor columns in `x` (each inner vec a predictor),
/// with an intercept added. Verified against numpy/statsmodels reference.
pub fn ols_fit(y: &[f64], x: &[Vec<f64>], names: &[String]) -> Option<OlsFit> {
    let n = y.len();
    let p = x.len(); // number of predictors (excl. intercept)
    if n == 0 || x.iter().any(|c| c.len() != n) {
        return None;
    }
    if !(n > p + 1) {
        return None;
    }
    // design matrix rows: [1, x1, x2, ...]
    let k = p + 1;
    let mut xmat: Vec<Vec<f64>> = Vec::with_capacity(n);
    for i in 0..n {
        let mut row = Vec::with_capacity(k);
        row.push(1.0);
        for c in x {
            row.push(c[i]);
        }
        xmat.push(row);
    }
    // X'X (k x k) and X'y (k)
    let mut xtx = vec![vec![0.0; k]; k];
    let mut xty = vec![0.0; k];
    for i in 0..n {
        for a in 0..k {
            xty[a] += xmat[i][a] * y[i];
            for b in 0..k {
                xtx[a][b] += xmat[i][a] * xmat[i][b];
            }
        }
    }
    let inv = gauss_jordan_inverse(&xtx)?;
    let beta: Vec<f64> = (0..k)
        .map(|a| (0..k).map(|b| inv[a][b] * xty[b]).sum())
        .collect();
    // residual sum of squares (ssr) and total sum of squares (sst).
    let mut ssr = 0.0;
    let ybar = y.iter().sum::<f64>() / n as f64;
    let mut sst = 0.0;
    for i in 0..n {
        let pred: f64 = (0..k).map(|a| beta[a] * xmat[i][a]).sum();
        ssr += (y[i] - pred).powi(2);
        sst += (y[i] - ybar).powi(2);
    }
    let df_resid = (n - k) as f64;
    if df_resid <= 0.0 || sst <= 0.0 {
        return None;
    }
    let sigma2 = ssr / df_resid;
    let se: Vec<f64> = (0..k).map(|a| (sigma2 * inv[a][a]).sqrt()).collect();
    let dist = StudentsT::new(0.0, 1.0, df_resid).ok()?;
    let mut t_value = vec![0.0; k];
    let mut p_value = vec![0.0; k];
    for a in 0..k {
        t_value[a] = beta[a] / se[a];
        p_value[a] = (2.0 * (1.0 - dist.cdf(t_value[a].abs()))).clamp(0.0, 1.0);
    }
    let r2 = 1.0 - ssr / sst;
    let adj = 1.0 - (1.0 - r2) * (n as f64 - 1.0) / df_resid;
    let f_stat = (r2 / p as f64) / ((1.0 - r2) / df_resid);
    let f_pvalue = match FisherSnedecor::new(p as f64, df_resid) {
        Ok(d) => (1.0 - d.cdf(f_stat)).clamp(0.0, 1.0),
        Err(_) => f64::NAN,
    };
    let mut terms = vec!["(Intercept)".to_string()];
    for i in 0..p {
        terms.push(
            names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("x{}", i + 1)),
        );
    }
    Some(OlsFit {
        terms,
        estimate: beta,
        std_error: se,
        t_value,
        p_value,
        r_squared: r2,
        adj_r_squared: adj,
        f_statistic: f_stat,
        f_pvalue,
        residual_std_error: sigma2.sqrt(),
        df_resid,
    })
}

/// Pearson correlation between two equal-length slices (None if degenerate).
pub fn pearson_corr(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() != y.len() || x.len() < 2 {
        return None;
    }
    let (mx, my) = (mean(x), mean(y));
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx).powi(2);
        syy += (b - my).powi(2);
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return None;
    }
    Some((sxy / (sxx * syy).sqrt()).clamp(-1.0, 1.0))
}

/// Spearman correlation (Pearson on the ranks).
pub fn spearman_corr(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() != y.len() || x.len() < 2 {
        return None;
    }
    pearson_corr(&rankdata(x), &rankdata(y))
}

/// Descriptive summary for one numeric column: (n, mean, sd, median, min, max).
pub fn describe(xs: &[f64]) -> Option<(usize, f64, f64, f64, f64, f64)> {
    if xs.is_empty() {
        return None;
    }
    let n = xs.len();
    let m = mean(xs);
    let sd = if n >= 2 { var_samp(xs, m).sqrt() } else { f64::NAN };
    let mut sorted = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    Some((n, m, sd, median, sorted[0], sorted[n - 1]))
}

/// A single histogram bin (0-based index, half-open [lower, upper) except the
/// last which is closed).
pub struct Bin {
    pub index: i64,
    pub lower: f64,
    pub upper: f64,
    pub count: i64,
}

/// Histogram binning. method: "equal" (equal-width, `bins` bins), "sturges",
/// or "fd" (Freedman-Diaconis). Returns one Bin per interval.
pub fn bin_edges(sample: &[f64], method: &str, bins: i64) -> Option<Vec<Bin>> {
    if sample.len() < 2 {
        return None;
    }
    let mut xs = sample.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    let lo = xs[0];
    let hi = xs[n - 1];
    if !(hi > lo) {
        return None;
    }
    let nbins = match method.to_ascii_lowercase().as_str() {
        "sturges" => ((n as f64).log2().ceil() as i64 + 1).max(1),
        "fd" | "freedman-diaconis" => {
            // IQR-based bin width -> count.
            let q = |p: f64| -> f64 {
                let pos = p * (n as f64 - 1.0);
                let i = pos.floor() as usize;
                let frac = pos - i as f64;
                if i + 1 < n {
                    xs[i] * (1.0 - frac) + xs[i + 1] * frac
                } else {
                    xs[i]
                }
            };
            let iqr = q(0.75) - q(0.25);
            if iqr <= 0.0 {
                ((n as f64).log2().ceil() as i64 + 1).max(1)
            } else {
                let width = 2.0 * iqr / (n as f64).cbrt();
                (((hi - lo) / width).ceil() as i64).max(1)
            }
        }
        _ => bins.max(1),
    };
    let nbins = nbins.max(1) as usize;
    let width = (hi - lo) / nbins as f64;
    let mut counts = vec![0_i64; nbins];
    for &v in &xs {
        let mut idx = ((v - lo) / width).floor() as i64;
        if idx < 0 {
            idx = 0;
        }
        if idx >= nbins as i64 {
            idx = nbins as i64 - 1; // include the max in the last bin
        }
        counts[idx as usize] += 1;
    }
    Some(
        (0..nbins)
            .map(|i| Bin {
                index: i as i64,
                lower: lo + i as f64 * width,
                upper: if i + 1 == nbins {
                    hi
                } else {
                    lo + (i + 1) as f64 * width
                },
                count: counts[i],
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected {b}, got {a}");
    }
    fn field(v: &Value, k: &str) -> f64 {
        v.get(k).unwrap().as_f64().unwrap()
    }

    #[test]
    fn t_one_sample() {
        // scipy: ttest_1samp([5,6,7,8,9], 6) -> t=1.41421356, p=0.23019964
        let r = ttest_1samp(&[5.0, 6.0, 7.0, 8.0, 9.0], 6.0, "two-sided").unwrap();
        approx(field(&r, "t_statistic"), 1.4142135, 1e-5);
        approx(field(&r, "p_value"), 0.2301996, 1e-5);
        approx(field(&r, "df"), 4.0, 1e-9);
    }

    #[test]
    fn t_welch_two_sample() {
        // scipy: ttest_ind([1,2,3,4,5],[2,4,6,8,10], equal_var=False)
        // -> t=-1.8973666, df=5.882353, p=0.10753119
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [2.0, 4.0, 6.0, 8.0, 10.0];
        let r = ttest_2samp(&a, &b, false, "two-sided").unwrap();
        approx(field(&r, "t_statistic"), -1.8973666, 1e-5);
        approx(field(&r, "df"), 5.882353, 1e-5);
        approx(field(&r, "p_value"), 0.10753119, 1e-5);
    }

    #[test]
    fn t_pooled_two_sample() {
        // scipy: ttest_ind([1,2,3,4,5],[2,4,6,8,10], equal_var=True)
        // -> t=-1.8973666, df=8, p=0.09434977
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [2.0, 4.0, 6.0, 8.0, 10.0];
        let r = ttest_2samp(&a, &b, true, "two-sided").unwrap();
        approx(field(&r, "t_statistic"), -1.8973666, 1e-5);
        approx(field(&r, "p_value"), 0.09434977, 1e-5);
        approx(field(&r, "df"), 8.0, 1e-9);
    }

    #[test]
    fn t_paired() {
        // scipy: ttest_rel([1,2,3,4,5],[2,3,4,5,7]) -> t=-6.0, p=0.0038825
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [2.0, 3.0, 4.0, 5.0, 7.0];
        let r = ttest_paired(&a, &b, "two-sided").unwrap();
        approx(field(&r, "t_statistic"), -6.0, 1e-6);
        approx(field(&r, "p_value"), 0.0038825, 1e-5);
    }

    #[test]
    fn pearson() {
        // scipy: pearsonr([1,2,3,4,5],[2,4,5,4,5]) -> r=0.7745967, p=0.12402706
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [2.0, 4.0, 5.0, 4.0, 5.0];
        let r = pearson_test(&x, &y, "two-sided").unwrap();
        approx(field(&r, "r"), 0.7745967, 1e-6);
        approx(field(&r, "p_value"), 0.12402706, 1e-5);
    }

    #[test]
    fn spearman() {
        // scipy: spearmanr([1,2,3,4,5],[5,6,7,8,7]) -> rho=0.8207826, p=0.0885870
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [5.0, 6.0, 7.0, 8.0, 7.0];
        let r = spearman_test(&x, &y, "two-sided").unwrap();
        approx(field(&r, "rho"), 0.8207826, 1e-6);
        approx(field(&r, "p_value"), 0.0885870, 1e-5);
    }

    #[test]
    fn anova() {
        // scipy: f_oneway([1,2,3],[4,5,6],[7,8,9]) -> F=27.0, p=0.000999
        let g = vec![
            vec![1.0, 2.0, 3.0],
            vec![4.0, 5.0, 6.0],
            vec![7.0, 8.0, 9.0],
        ];
        let r = anova_oneway(&g).unwrap();
        approx(field(&r, "f_statistic"), 27.0, 1e-6);
        approx(field(&r, "p_value"), 0.0009990, 1e-5);
    }

    #[test]
    fn chisq_gof() {
        // scipy: chisquare([16,18,16,14,12,12]) -> chi2=2.0, p=0.84914504
        let obs = [16.0, 18.0, 16.0, 14.0, 12.0, 12.0];
        let r = chisq_goodness_of_fit(&obs, None).unwrap();
        approx(field(&r, "chi_square"), 2.0, 1e-9);
        approx(field(&r, "p_value"), 0.84914504, 1e-5);
    }

    #[test]
    fn chisq_indep() {
        // scipy: chi2_contingency([[10,20],[30,40]], correction=False)
        // -> chi2=0.79365079, p=0.37299848
        let t = vec![vec![10.0, 20.0], vec![30.0, 40.0]];
        let r = chisq_independence(&t, false).unwrap();
        approx(field(&r, "chi_square"), 0.79365079, 1e-6);
        approx(field(&r, "p_value"), 0.37299848, 1e-5);
        approx(field(&r, "df"), 1.0, 1e-9);
        // with Yates' correction -> chi2=0.44642857, p=0.50403587
        let ry = chisq_independence(&t, true).unwrap();
        approx(field(&ry, "chi_square"), 0.44642857, 1e-6);
        approx(field(&ry, "p_value"), 0.50403587, 1e-5);
    }

    #[test]
    fn mann_whitney() {
        // scipy: mannwhitneyu([1,2,3,4,5],[6,7,8,9,10]) -> U=0.0
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [6.0, 7.0, 8.0, 9.0, 10.0];
        let r = mann_whitney_u(&a, &b, "two-sided", false).unwrap();
        approx(field(&r, "u_statistic"), 0.0, 1e-9);
        // normal-approx two-sided p ~ 0.0090 (scipy exact 0.0079).
        assert!(field(&r, "p_value") < 0.02);
    }

    #[test]
    fn wilcoxon() {
        // R/scipy wilcoxon([1,2,3,4,10],[2,4,6,8,9]) signed-rank.
        let a = [1.0, 2.0, 3.0, 4.0, 10.0];
        let b = [2.0, 4.0, 6.0, 8.0, 9.0];
        let r = wilcoxon_signed_rank(&a, &b, "two-sided", false).unwrap();
        // W = min(W+, W-); differences -1,-2,-3,-4,+1 -> ranks, W+ small.
        assert!(field(&r, "w_statistic") >= 0.0);
    }

    #[test]
    fn sign_test() {
        // 7 of 10 above mu, binomial two-sided p = 0.34375.
        let x = [2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, -1.0, -1.0, -1.0];
        let r = sign_test_1samp(&x, 0.0, "two-sided").unwrap();
        approx(field(&r, "n_pos"), 7.0, 1e-9);
        approx(field(&r, "n_neg"), 3.0, 1e-9);
        approx(field(&r, "p_value"), 0.34375, 1e-6);
    }

    #[test]
    fn jarque_bera_normalish() {
        // scipy jarque_bera([1,2,3,4,5,4,3,2]) -> JB=0.3333333, p=0.84648172
        let x = [1.0, 2.0, 3.0, 4.0, 5.0, 4.0, 3.0, 2.0];
        let r = jarque_bera(&x).unwrap();
        approx(field(&r, "jb_statistic"), 0.3333333, 1e-6);
        approx(field(&r, "p_value"), 0.84648172, 1e-5);
        approx(field(&r, "skewness"), 0.0, 1e-9);
    }

    #[test]
    fn ks_2samp() {
        // identical-shape, shifted samples -> D large, p small.
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [11.0, 12.0, 13.0, 14.0, 15.0];
        let r = ks_test_2samp(&a, &b).unwrap();
        approx(field(&r, "d_statistic"), 1.0, 1e-9);
        assert!(field(&r, "p_value") < 0.05);
    }

    #[test]
    fn adjust_bonferroni_and_bh() {
        let p = [0.01, 0.02, 0.03, 0.04, 0.05];
        let b = adjust_p(&p, "bonferroni").unwrap();
        approx(b[0], 0.05, 1e-9);
        approx(b[4], 0.25, 1e-9);
        // R p.adjust(c(.01,.02,.03,.04,.05),"BH") = 0.05 each.
        let bh = adjust_p(&p, "BH").unwrap();
        for v in bh {
            approx(v, 0.05, 1e-9);
        }
    }

    #[test]
    fn adjust_holm() {
        // R p.adjust(c(.01,.04,.03,.005),"holm")=c(.03,.06,.06,.02)
        let p = [0.01, 0.04, 0.03, 0.005];
        let h = adjust_p(&p, "holm").unwrap();
        approx(h[0], 0.03, 1e-9);
        approx(h[3], 0.02, 1e-9);
        approx(h[1], 0.06, 1e-9);
        approx(h[2], 0.06, 1e-9);
    }

    // ---- Priority 1: the hard tests -------------------------------------

    #[test]
    fn shapiro_normalish() {
        // scipy.stats.shapiro([2,4,5,7,8,9,11,12,14,15]) -> W=0.971567, p=0.904979
        let s = [2.0, 4.0, 5.0, 7.0, 8.0, 9.0, 11.0, 12.0, 14.0, 15.0];
        let r = shapiro_wilk(&s).unwrap();
        approx(field(&r, "W"), 0.971567, 1e-3);
        approx(field(&r, "p_value"), 0.904979, 5e-3);
    }

    #[test]
    fn shapiro_skewed() {
        // scipy.stats.shapiro([148,154,158,160,161,162,166,170,182,195,236])
        // -> W=0.788815, p=0.006704 (n=11, the <=11 transform branch)
        let s = [
            148.0, 154.0, 158.0, 160.0, 161.0, 162.0, 166.0, 170.0, 182.0, 195.0, 236.0,
        ];
        let r = shapiro_wilk(&s).unwrap();
        approx(field(&r, "W"), 0.788815, 2e-3);
        approx(field(&r, "p_value"), 0.006704, 5e-3);
    }

    #[test]
    fn anderson_normalish() {
        // scipy.stats.anderson([2,4,5,7,8,9,11,12,14,15],'norm') statistic=0.140359
        // (scipy reports the A_star adjusted? -- it reports raw A2=0.140359;
        //  A_star=0.154044). Our piecewise p-value should be large (>0.5).
        let s = [2.0, 4.0, 5.0, 7.0, 8.0, 9.0, 11.0, 12.0, 14.0, 15.0];
        let r = anderson_darling(&s).unwrap();
        approx(field(&r, "A_squared"), 0.140359, 1e-4);
        approx(field(&r, "A_star"), 0.154044, 1e-4);
        assert!(field(&r, "p_value") > 0.5);
    }

    #[test]
    fn ks_normal() {
        // scipy.stats.ks_1samp(samp, norm.cdf, method='asymp')
        // -> D=0.100000, p=0.988261
        let samp = [
            0.1, 0.5, -0.3, 1.2, -1.1, 0.4, 0.8, -0.6, 0.2, -0.9, 1.5, -1.3, 0.7, 0.0, 0.3,
            -0.4, 0.9, -0.7, 1.1, -0.2,
        ];
        let params = json!({"mean": 0.0, "std": 1.0});
        let r = ks_test_1samp(&samp, "normal", &params).unwrap();
        approx(field(&r, "D"), 0.100000, 1e-6);
        approx(field(&r, "p_value"), 0.988261, 5e-3);
    }

    #[test]
    fn ks_uniform_and_expon() {
        // uniform(0,1): scipy D=0.100000, p=0.999965
        let su = [0.1, 0.3, 0.5, 0.7, 0.9, 0.2, 0.4, 0.6, 0.8, 0.15];
        let r = ks_test_1samp(&su, "uniform", &json!({"min":0.0,"max":1.0})).unwrap();
        approx(field(&r, "D"), 0.100000, 1e-6);
        assert!(field(&r, "p_value") > 0.95);
        // exponential rate=1: scipy D=0.150671, p=0.977042
        let se = [0.2, 0.5, 1.0, 1.5, 2.0, 0.3, 0.8, 1.2, 2.5, 0.1];
        let r = ks_test_1samp(&se, "exponential", &json!({"rate":1.0})).unwrap();
        approx(field(&r, "D"), 0.150671, 1e-5);
        assert!(field(&r, "p_value") > 0.9);
    }

    #[test]
    fn kendall_no_ties() {
        // scipy.stats.kendalltau([1..8],[2,1,4,3,6,5,8,7], method='asymptotic')
        // -> tau=0.714286, p=0.013348
        let x = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let y = [2.0, 1.0, 4.0, 3.0, 6.0, 5.0, 8.0, 7.0];
        let r = kendall_test(&x, &y).unwrap();
        approx(field(&r, "tau"), 0.714286, 1e-5);
        approx(field(&r, "p_value"), 0.013348, 1e-3);
    }

    #[test]
    fn kendall_with_ties() {
        // scipy kendalltau([1,1,2,2,3,3],[1,2,2,3,3,4], method='asymptotic')
        // -> tau=0.800641, p=0.040104
        let x = [1.0, 1.0, 2.0, 2.0, 3.0, 3.0];
        let y = [1.0, 2.0, 2.0, 3.0, 3.0, 4.0];
        let r = kendall_test(&x, &y).unwrap();
        approx(field(&r, "tau"), 0.800641, 1e-4);
        approx(field(&r, "p_value"), 0.040104, 5e-3);
    }

    #[test]
    fn poibin() {
        // direct DP reference: probs=[.1,.5,.9,.3,.7]
        // cdf k=0..5 = .009450,.131100,.500000,.868900,.990550,1.0
        let p = [0.1, 0.5, 0.9, 0.3, 0.7];
        approx(poibin_cdf(&p, 0).unwrap(), 0.009450, 1e-6);
        approx(poibin_cdf(&p, 1).unwrap(), 0.131100, 1e-6);
        approx(poibin_cdf(&p, 2).unwrap(), 0.500000, 1e-6);
        approx(poibin_cdf(&p, 3).unwrap(), 0.868900, 1e-6);
        assert_eq!(poibin_cdf(&p, 5).unwrap(), 1.0);
        assert!(poibin_cdf(&[1.5], 0).is_none());
    }

    // ---- Priority 2: table-function backends ----------------------------

    #[test]
    fn ols_simple() {
        // numpy OLS y on x with intercept:
        //   intercept=0.060000 se=0.108600 t=0.552487 p=0.59571
        //   slope=1.990909 se=0.017502 t=113.750247 p=3.99e-14
        //   r2=0.999382 adjr2=0.999305 F=12939.118705 rse=0.158974 df=8
        let xs = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let ys = [2.1, 3.9, 6.2, 7.8, 10.1, 12.2, 13.8, 16.1, 18.0, 19.9];
        let fit = ols_fit(&ys, &[xs], &["x".to_string()]).unwrap();
        approx(fit.estimate[0], 0.060000, 1e-4);
        approx(fit.std_error[0], 0.108600, 1e-4);
        approx(fit.estimate[1], 1.990909, 1e-5);
        approx(fit.std_error[1], 0.017502, 1e-5);
        approx(fit.t_value[1], 113.750247, 1e-2);
        approx(fit.r_squared, 0.999382, 1e-5);
        approx(fit.adj_r_squared, 0.999305, 1e-5);
        approx(fit.f_statistic, 12939.118705, 1e-1);
        approx(fit.residual_std_error, 0.158974, 1e-5);
        approx(fit.df_resid, 8.0, 1e-9);
        assert_eq!(fit.terms, vec!["(Intercept)", "x"]);
    }

    #[test]
    fn corr_pearson_spearman() {
        // numpy: corr([1..5],[2,4,5,4,5])=0.774597; ([1..5],[5,3,2,4,1])=-0.7
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [2.0, 4.0, 5.0, 4.0, 5.0];
        let c = [5.0, 3.0, 2.0, 4.0, 1.0];
        approx(pearson_corr(&a, &b).unwrap(), 0.774597, 1e-6);
        approx(pearson_corr(&a, &c).unwrap(), -0.700000, 1e-6);
        // scipy spearman([1..5],[2,4,5,4,5]) = 0.737865
        approx(spearman_corr(&a, &b).unwrap(), 0.737865, 1e-5);
    }

    #[test]
    fn describe_column() {
        // numpy: n=5 mean=3 sd=1.581139 median=3 min=1 max=5
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let (n, m, sd, med, lo, hi) = describe(&a).unwrap();
        assert_eq!(n, 5);
        approx(m, 3.0, 1e-9);
        approx(sd, 1.581139, 1e-6);
        approx(med, 3.0, 1e-9);
        approx(lo, 1.0, 1e-9);
        approx(hi, 5.0, 1e-9);
    }

    #[test]
    fn bins_sturges() {
        // numpy histogram(1..20, bins=sturges=6):
        //   counts [4,3,3,3,3,4], edges step 3.1667
        let samp: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        let bins = bin_edges(&samp, "sturges", 0).unwrap();
        assert_eq!(bins.len(), 6);
        let counts: Vec<i64> = bins.iter().map(|b| b.count).collect();
        assert_eq!(counts, vec![4, 3, 3, 3, 3, 4]);
        approx(bins[0].lower, 1.0, 1e-9);
        approx(bins[5].upper, 20.0, 1e-9);
        approx(bins[1].lower, 4.166667, 1e-4);
    }

    #[test]
    fn bins_equal_width() {
        // 0..10 into 5 equal bins -> each width 2.
        let samp = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let bins = bin_edges(&samp, "equal", 5).unwrap();
        assert_eq!(bins.len(), 5);
        approx(bins[0].upper - bins[0].lower, 2.0, 1e-9);
        let total: i64 = bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 11);
    }
}
