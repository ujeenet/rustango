-- postgres
[op 0] tx
  CREATE TABLE "s3_defaults" ("id" BIGINT NOT NULL, "s" VARCHAR(20) DEFAULT 'en', "q" VARCHAR(20) DEFAULT 'it''s', "i" BIGINT DEFAULT 0, "d" NUMERIC DEFAULT 0.00, "b" BOOLEAN DEFAULT TRUE, "dt" DATE DEFAULT '2026-01-01', "ts" TIMESTAMPTZ DEFAULT now(), "u" UUID DEFAULT gen_random_uuid(), "j" JSONB DEFAULT '{}'::jsonb, "ja" JSONB DEFAULT '[]'::jsonb, PRIMARY KEY ("id"));
-- mysql
[op 0] autocommit
  CREATE TABLE `s3_defaults` (`id` BIGINT NOT NULL, `s` VARCHAR(20) DEFAULT 'en', `q` VARCHAR(20) DEFAULT 'it''s', `i` BIGINT DEFAULT 0, `d` DECIMAL(65, 28) DEFAULT 0.00, `b` TINYINT(1) DEFAULT TRUE, `dt` DATE DEFAULT '2026-01-01', `ts` DATETIME(6) DEFAULT (UTC_TIMESTAMP(6)), `lt` LONGTEXT DEFAULT ('en'), `u` CHAR(36) DEFAULT (UUID()), `j` JSON DEFAULT ('{}'), `ja` JSON DEFAULT ('[]'), PRIMARY KEY (`id`));
-- sqlite
[op 0] tx
  CREATE TABLE "s3_defaults" ("id" INTEGER NOT NULL, "s" TEXT DEFAULT 'en', "q" TEXT DEFAULT 'it''s', "i" INTEGER DEFAULT 0, "d" NUMERIC DEFAULT 0.00, "b" INTEGER DEFAULT TRUE, "dt" TEXT DEFAULT '2026-01-01', "ts" TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000+00:00','now')), "u" TEXT DEFAULT (unhex(hex(randomblob(6)) || '4' || substr(hex(randomblob(2)), 2) || substr('89AB', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(8)), 2))), "j" TEXT DEFAULT '{}', "ja" TEXT DEFAULT '[]', PRIMARY KEY ("id"));
