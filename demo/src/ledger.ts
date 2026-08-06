const entries = new Map<string, number>();

export const ledger = {
  record(id: string, amount: number) {
    entries.set(id, (entries.get(id) ?? 0) + amount);
    return entries.get(id);
  },
  read(id: string) {
    return entries.get(id) ?? 0;
  },
};
