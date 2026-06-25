-- statsduck smoke: hypothesis tests + distribution CDFs.
-- Results are JSON objects; we extract a field, cast, and round so the
-- assertions are deterministic. Reference values come from scipy/R.
-- one-sample t-test t([5,6,7,8,9], mu=6): t=1.4142, p=0.2302 (scipy).
SELECT round(CAST(json_extract(ttest_1samp('[5,6,7,8,9]', 6.0, 'two-sided'), '$.t_statistic') AS DOUBLE), 4) AS t1_t;
SELECT round(CAST(json_extract(ttest_1samp('[5,6,7,8,9]', 6.0, 'two-sided'), '$.p_value') AS DOUBLE), 4) AS t1_p;
-- Welch two-sample t([1..5],[2,4,6,8,10]): t=-1.8974, p=0.1075 (scipy).
SELECT round(CAST(json_extract(ttest_2samp('[1,2,3,4,5]', '[2,4,6,8,10]', false, 'two-sided'), '$.t_statistic') AS DOUBLE), 4) AS t2_t;
SELECT round(CAST(json_extract(ttest_2samp('[1,2,3,4,5]', '[2,4,6,8,10]', false, 'two-sided'), '$.p_value') AS DOUBLE), 4) AS t2_p;
-- paired t([1..5],[2,3,4,5,7]): t=-6.0, p=0.0039 (scipy).
SELECT round(CAST(json_extract(ttest_paired('[1,2,3,4,5]', '[2,3,4,5,7]', 'two-sided'), '$.t_statistic') AS DOUBLE), 4) AS tp_t;
SELECT round(CAST(json_extract(ttest_paired('[1,2,3,4,5]', '[2,3,4,5,7]', 'two-sided'), '$.p_value') AS DOUBLE), 4) AS tp_p;
-- Pearson r([1..5],[2,4,5,4,5]): r=0.7746, p=0.1240 (scipy).
SELECT round(CAST(json_extract(pearson_test('[1,2,3,4,5]', '[2,4,5,4,5]', 'two-sided'), '$.r') AS DOUBLE), 4) AS pe_r;
SELECT round(CAST(json_extract(pearson_test('[1,2,3,4,5]', '[2,4,5,4,5]', 'two-sided'), '$.p_value') AS DOUBLE), 4) AS pe_p;
-- Spearman rho([1..5],[5,6,7,8,7]): rho=0.8208 (scipy).
SELECT round(CAST(json_extract(spearman_test('[1,2,3,4,5]', '[5,6,7,8,7]', 'two-sided'), '$.rho') AS DOUBLE), 4) AS sp_rho;
-- one-way ANOVA: F=27.0, p=0.0010 (scipy f_oneway).
SELECT round(CAST(json_extract(anova_oneway('[[1,2,3],[4,5,6],[7,8,9]]'), '$.f_statistic') AS DOUBLE), 4) AS an_f;
SELECT round(CAST(json_extract(anova_oneway('[[1,2,3],[4,5,6],[7,8,9]]'), '$.p_value') AS DOUBLE), 4) AS an_p;
-- chi-square goodness-of-fit [16,18,16,14,12,12]: chi2=2.0, p=0.8491 (scipy).
SELECT round(CAST(json_extract(chisq_goodness_of_fit('[16,18,16,14,12,12]', ''), '$.chi_square') AS DOUBLE), 4) AS gof_x;
SELECT round(CAST(json_extract(chisq_goodness_of_fit('[16,18,16,14,12,12]', ''), '$.p_value') AS DOUBLE), 4) AS gof_p;
-- chi-square independence [[10,20],[30,40]] no correction: chi2=0.7937, p=0.3730 (scipy).
SELECT round(CAST(json_extract(chisq_independence('[[10,20],[30,40]]', false), '$.chi_square') AS DOUBLE), 4) AS in_x;
SELECT round(CAST(json_extract(chisq_independence('[[10,20],[30,40]]', false), '$.p_value') AS DOUBLE), 4) AS in_p;
-- Mann-Whitney U([1..5],[6..10]): U=0 (scipy).
SELECT round(CAST(json_extract(mann_whitney_u('[1,2,3,4,5]', '[6,7,8,9,10]', 'two-sided', false), '$.u_statistic') AS DOUBLE), 4) AS mwu;
-- Jarque-Bera symmetric sample: skew=0, p=0.8465 (scipy).
SELECT round(CAST(json_extract(jarque_bera('[1,2,3,4,5,4,3,2]'), '$.p_value') AS DOUBLE), 4) AS jb_p;
-- two-sample KS, disjoint samples: D=1.0 (scipy).
SELECT round(CAST(json_extract(ks_test_2samp('[1,2,3,4,5]', '[11,12,13,14,15]'), '$.d_statistic') AS DOUBLE), 4) AS ks_d;
-- Bonferroni adjust of [.01,.02,.03,.04,.05] -> first element 0.05.
SELECT round(CAST(json_extract(adjust_p('[0.01,0.02,0.03,0.04,0.05]', 'bonferroni'), '$[0]') AS DOUBLE), 4) AS adj0;
-- distribution CDFs: t_cdf(0,5)=0.5, chisq_cdf(3.357,5)=0.5, f_cdf(1,10,10)=0.5.
SELECT round(t_cdf(0.0, 5.0), 4) AS tcdf;
SELECT round(chisq_cdf(4.3515, 5.0), 4) AS ccdf;
SELECT round(f_cdf(1.0, 10.0, 10.0), 4) AS fcdf;
-- gamma_cdf(1; shape=1, rate=1) = 1 - e^-1 = 0.6321; weibull(1;1,1) same.
SELECT round(gamma_cdf(1.0, 1.0, 1.0), 4) AS gcdf;
SELECT round(weibull_cdf(1.0, 1.0, 1.0), 4) AS wcdf;
-- lognormal_cdf(1; 0, 1) = 0.5 (median at exp(0)=1).
SELECT round(lognormal_cdf(1.0, 0.0, 1.0), 4) AS lcdf;
-- NULL input propagates to NULL.
SELECT ttest_1samp(NULL, 6.0, 'two-sided') AS nullin;
-- malformed JSON yields NULL (no panic).
SELECT ttest_1samp('not json', 6.0, 'two-sided') AS badjson;
-- too few observations yields NULL.
SELECT ttest_1samp('[5]', 6.0, 'two-sided') AS toofew;
