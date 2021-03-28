// @ts-ignore
import { Console } from "as-wasi";
import { Method, RequestBuilder, Response } from "../../crates/as";
export { alloc } from "../../crates/as";

export function post(): void {
  let body = String.UTF8.encode("testing the body");
  let res = new RequestBuilder("https://postman-echo.com/post")
    .header("Content-Type", "text/plain")
    .header("abc", "def")
    .method(Method.POST)
    .body(body)
    .send();

  check(res, 200, "Content-Type");
}

export function get(): void {
  let res = new RequestBuilder("https://api.brigade.sh/healthz")
    .method(Method.GET)
    .send();

  check(res, 200, "Content-Type");
  let body = res.bodyReadAll();
  if (String.UTF8.decode(body.buffer) != '"OK"') {
    abort();
  }
}

function check(
  res: Response,
  expectedStatus: u32,
  expectedHeader: string
): void {
  if (res.status != expectedStatus) {
    Console.write(
      "expected status " +
      expectedStatus.toString() +
      " got " +
      res.status.toString()
    );
    abort();
  }

  let headerValue = res.headerGet(expectedHeader);
  if (!headerValue) {
    abort();
  }
}
