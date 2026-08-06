import { ledger } from "./ledger";

export class PaymentService {
  async processPayment(id: string, amount: number) {
    const fee = computeFee(amount);
    return ledger.record(id, amount + fee);
  }

  async refund(id: string, amount: number) {
    return ledger.record(id, -amount);
  }

  async chargeback(id: string) {
    return ledger.record(id, 0);
  }
}

export function computeFee(amount: number) {
  return Math.round(amount * 0.029) + 30;
}

export function audit(id: string) {
  return ledger.read(id);
}
