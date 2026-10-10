-- mysql: paid orders report
SELECT id, status, total
FROM order
WHERE status = 'paid';
