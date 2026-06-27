import { add } from "./math.ts";

export function main(): void {
  const result = add(2, 3);

  console.log(`2 + 3 = ${result.toString()}`);
}

if (import.meta.main) {
  main();
}
