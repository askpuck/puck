export type RoleId = "coordinatore" | "coder";

export type Role = {
  id: RoleId;
  name: string;
  trade: string;
  alwaysOn: boolean;
};

export const CREW: Role[] = [
  {
    id: "coordinatore",
    name: "Coordinator",
    trade: "Takes the order. Wakes the Coder when needed. Reports back.",
    alwaysOn: true,
  },
  {
    id: "coder",
    name: "Coder",
    trade: "Builds code, files, pages — including the words. Does not publish.",
    alwaysOn: false,
  },
];
