// A WIT component written in plain JavaScript, compiled with componentize-qjs.
import { log, emit } from 'tauri:backend/host@0.1.0';
// Host commands imported in wit/world.wit: typed, no JSON strings.
import { appVersion } from 'example:greeter/app';

let count = 0;

export function greet(name) {
  log('info', `greeting ${name}`);
  emit('greeter://greeted', JSON.stringify({ name }));
  return `Hello, ${name}! You've been greeted from JavaScript running in WASM!`;
}

// Returning `result<string, string>`: a thrown string becomes `err`.
export function describe(person) {
  if (!person.name) throw 'a person needs a name';
  const version = appVersion();
  return `${person.name} is ${person.ageYears} years old (app v${version})`;
}

export function increment(by) {
  count += by;
  return count;
}

export const events = {
  // Receives the events listed in `capabilities.events.listen`.
  onEvent(event, payload) {
    log('info', `received ${event}: ${payload}`);
    emit('greeter://pong', JSON.stringify({ count, received: JSON.parse(payload) }));
  },
};
