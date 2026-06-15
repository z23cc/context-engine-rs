export class SharedThing {
  constructor(value) {
    this.value = value;
  }
}

export function sharedFactory() {
  return new SharedThing(1);
}
