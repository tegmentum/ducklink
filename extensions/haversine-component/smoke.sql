-- haversine extension smoke (NYC -> LA ~ 3936 km / 2446 mi).
SELECT round(haversine_km(40.7128, -74.0060, 34.0522, -118.2437), 0) AS km;
SELECT round(haversine_mi(40.7128, -74.0060, 34.0522, -118.2437), 0) AS mi;
SELECT round(haversine_km(0, 0, 0, 0), 4) AS zero;
SELECT haversine_km(1, 2, 3, NULL) AS nullarg;
