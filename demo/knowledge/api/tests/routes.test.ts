import { createPayment, getPayment } from '../src/routes';

export function testCreatePayment() {
  return createPayment({ amount: 100 });
}

export function testGetPayment() {
  return getPayment({ id: 'pay_1' });
}
