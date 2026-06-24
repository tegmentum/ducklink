-- stochastic extension smoke: distribution PDF/PMF, CDF and quantile scalars.
-- normal: cdf(0)=0.5, pdf(0)=1/sqrt(2pi)=0.3989, quantile(0.975)=1.96.
SELECT round(normal_cdf(0, 0, 1), 4) AS ncdf;
SELECT round(normal_pdf(0, 0, 1), 4) AS npdf;
SELECT round(normal_quantile(0.975, 0, 1), 2) AS nq;
-- binomial P(X=2; n=5, p=0.5) = 10/32 = 0.3125.
SELECT round(binomial_pmf(2, 5, 0.5), 4) AS bpmf;
-- poisson P(X=2; lambda=3).
SELECT round(poisson_pmf(2, 3.0), 4) AS ppmf;
-- exponential CDF(1; rate=1) = 1 - e^-1.
SELECT round(exponential_cdf(1, 1), 4) AS ecdf;
-- beta CDF(0.5; 2, 2) = 0.5 by symmetry.
SELECT round(beta_cdf(0.5, 2, 2), 4) AS becdf;
-- NULL input propagates to NULL.
SELECT normal_cdf(NULL, 0, 1) AS nullin;
-- invalid param sd<=0 yields NULL (no panic).
SELECT normal_cdf(0, 0, 0) AS badsd;
