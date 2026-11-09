# Common tasks

Short answers to "how do I …?". Each answer is one line and links to the page
with the code. The pages assume the app from
[Getting started](getting-started.md).

Some tasks have no complete, copy-and-run example yet. Those rows link the
closest page and say "(complete example coming)".

## Table of contents

- [Tasks](#tasks)
- [More recipes: the cookbook](#more-recipes-the-cookbook)

## Tasks

| Task | Answer | Read |
|---|---|---|
| Sign up a user | Check the password with `passwords::strength_score`, hash it with `passwords::hash`, and store the hash on the user row. | [Getting started, Step 14](getting-started.md#step-14-add-sign-up-login-and-jwt-authentication) · [Hashing on signup](auth-passwords.md#hashing-on-signup) |
| Log in | Check the password with `passwords::verify`, then start a session or issue a JWT. | [Getting started, Step 14](getting-started.md#step-14-add-sign-up-login-and-jwt-authentication) · [Verifying on login](auth-passwords.md#verifying-on-login) · [Sessions](auth-sessions.md) |
| Reset a password | `PasswordReset::issue` makes a signed link to email; `confirm_password_reset_pool` checks it and stores the new hash. | [Password reset](auth-flows.md#password-reset) (complete example coming) |
| Upload a file | `save_uploads` checks the file and writes it to a `Storage`; you save the returned key on a model. | [Upload an avatar, end to end](files.md#upload-an-avatar-end-to-end) |
| Send an email | Build an `Email`, pick a mailer, and call `email.send(&mailer)`. | [Sending email](email.md#step-1--build-an-email) |
| Run a background job | Implement `Job`, register it on a queue, start the queue, and `dispatch` from a handler. | [Background jobs](jobs.md#step-1--define-a-job) |
| Test a view | Wrap your router in `TestClient` and send requests in process, with no server. | [Testing](testing.md#step-1--drive-your-app-with-testclient) |
| Deploy | Set `RUSTANGO_ENV=prod`, run `migrate` and `check --deploy`, then build with `cargo build --release`. | [Getting started, Step 18](getting-started.md#step-18-deploy-to-production) · [Auditing before you deploy](security.md#auditing-before-you-deploy) |

## More recipes: the cookbook

The [cookbook](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/cookbook_blog/COOKBOOK.md)
is a runnable blog app with recipes for models, the ORM, migrations, the
admin, REST APIs, multi-tenancy and more. The recipes are backed by tests.
For multi-tenancy itself, start with [Multi-tenancy](tenancy.md).
