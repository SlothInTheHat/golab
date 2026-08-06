import { record, balance } from '../../lib/src/ledger';

export function registerRoutes(app) {
  app.post('/payments', createPayment);
  app.get('/payments/:id', getPayment);
  app.delete('/payments/:id', voidPayment);
}

export function createPayment(req) {
  const id = db.query("INSERT INTO payments (amount) VALUES ($1) RETURNING id", [
    req.amount,
  ]);
  return record(id, req.amount);
}

export function getPayment(req) {
  return db.query("SELECT * FROM payments WHERE id = $1", [req.id]);
}

export function voidPayment(req) {
  return record(req.id, -balance(req.id));
}
