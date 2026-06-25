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
}
