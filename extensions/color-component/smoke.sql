-- color extension smoke (WCAG: white luminance=1, black=0, contrast=21).
SELECT round(color_luminance('white'), 4) AS lum_white;
SELECT round(color_luminance('black'), 4) AS lum_black;
SELECT round(color_contrast('white', 'black'), 1) AS max_contrast;
SELECT round(color_contrast('#777', '#fff'), 2) AS gray_on_white;
SELECT color_luminance('not-a-color') AS bad;
