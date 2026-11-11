# The admin pages in the browser

The server can serve a small set of admin pages under `/web`, so an administrator can look after the server without the
Craft Cloud app. They are plain HTML, with no script, and they are off until you turn them on. [`server.md`](server.md)
("The admin pages in the browser") explains how to turn them on behind a TLS proxy, the sessions, and what the audit log
records. This page shows what each page does. The screenshots come from a test server with made-up accounts.

Only administrators can sign in. Every change asks for the administrator's password again, and for a two-factor code when
the account uses two-factor sign-in. The pages follow the browser's light or dark setting and fit a phone's width.

## Overview

![The overview, light](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/overview-1280-light.png)

The overview shows the server's health (the database, the store, and whether uploads are refused because the server is
full), the number of accounts, signed-in computers, stored bytes and live documents, and the server's version, store and
database kind. It links to the other pages.

When the update check is turned on (it is off by default) and has found a newer Craft Cloud, a banner at the top gives the version, a link to the release notes (or
the comparison, for a branch check) and the command that updates a Docker install. The server never updates itself. See
"Data, upgrading and backups" in [`server.md`](server.md#data-upgrading-and-backups) for the check and the script.

![The overview, dark](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/overview-1280-dark.png)

## Users

![Users, light](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/users-1280-light.png)

The list of accounts, with a search by name and paging. A new account gets an invite code, shown once, to hand to its owner,
who chooses the password when redeeming it. An account's own page has its role and quota, disabling, enabling and removing
it, invite and password reset codes, turning its two-factor sign-in off, and its devices, which you can sign out one by one.

![An account, light](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/account-1280-light.png)

## Team spaces

![Team spaces, light](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/spaces-1280-light.png)

The team spaces of the server with their owners, members, use against the limit and state. Here an administrator makes a
space and names its first owner. A space's page shows its members and roles (read only), and has forms for its limit,
archiving and unarchiving it, and naming an owner. Owners invite and remove members from the Craft Cloud app. An
administrator has no access to a team space's files.

![A team space, dark](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/space-1280-dark.png)

## Storage

Stored bytes, the configured capacity and how much of it is used, the file systems of the store and of the data directory,
the state of the manifest index, and the ten largest accounts. Below them are the compression settings and the last estimate.
The form there sets the compression level (1 to 19) for new files, the same change as `PUT /admin/storage/compression`.
Opening the page does not run an estimate. The server does not measure the physical size and file count per account yet, so an
account's page says "not measured" there.

## Audit log

![The audit log, light](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/audit-1280-light.png)

The security events, newest first, with a filter by account ID and by event or event group, and a download of the filtered
list as NDJSON. Accounts appear by ID, and the log holds no password, code or address.

## On a phone

At 390 px the header and navigation wrap, and tables scroll inside their own frame.

![The overview on a phone](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/overview-390-light.png)
