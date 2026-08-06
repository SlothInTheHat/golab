export function record(paymentId: string, delta: number) {
  return db.query(
    "INSERT INTO ledger_entries (payment_id, delta) VALUES ($1, $2)",
    [paymentId, delta],
  );
}

export function balance(paymentId: string) {
  return db.query("SELECT SUM(delta) FROM ledger_entries WHERE payment_id = $1", [
    paymentId,
  ]);
}
