-- weekly report: customers with their order totals
SELECT c.name, o.total
FROM customers c, orders
WHERE c.id = orders.customer_id;
