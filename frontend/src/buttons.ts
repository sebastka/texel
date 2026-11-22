// Controller buttons, as bits of the masks sent to the emulator (1 = pressed).

export const Button = {
  Select: 1 << 0,
  L3: 1 << 1,
  R3: 1 << 2,
  Start: 1 << 3,
  Up: 1 << 4,
  Right: 1 << 5,
  Down: 1 << 6,
  Left: 1 << 7,
  L2: 1 << 8,
  R2: 1 << 9,
  L1: 1 << 10,
  R1: 1 << 11,
  Triangle: 1 << 12,
  Circle: 1 << 13,
  Cross: 1 << 14,
  Square: 1 << 15,
} as const;
