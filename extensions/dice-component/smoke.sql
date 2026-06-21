-- dice extension smoke (roll is random; assert bounds, not value).
SELECT dice_min('2d6+3') AS min;
SELECT dice_max('2d6+3') AS max;
SELECT dice_max('d20') AS d20_max;
SELECT dice_roll('3d6') BETWEEN 3 AND 18 AS in_range;
SELECT dice_min('garbage') AS bad;
