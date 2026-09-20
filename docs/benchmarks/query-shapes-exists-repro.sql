-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
--
-- See LICENSE in the repository root for license terms.

SET statement_timeout=120000;
SET jit=off;
CREATE TEMP TABLE shape_documents(id bigint PRIMARY KEY, body text NOT NULL);
INSERT INTO shape_documents SELECT n, string_agg(w, ' ' ORDER BY j) || CASE WHEN n % 97 = 0 THEN ' raretoken' ELSE '' END FROM generate_series(1,4096) n CROSS JOIN unnest(ARRAY['wordaa','wordab','wordac','wordad','wordae','wordaf','wordag','wordah','wordai','wordaj','wordak','wordal','wordam','wordan','wordao','wordap','wordaq','wordar','wordas','wordat','wordau','wordav','wordaw','wordax','worday','wordaz','wordba','wordbb','wordbc','wordbd','wordbe','wordbf','wordbg','wordbh','wordbi','wordbj','wordbk','wordbl','wordbm','wordbn','wordbo','wordbp','wordbq','wordbr','wordbs','wordbt','wordbu','wordbv','wordbw','wordbx','wordby','wordbz','wordca','wordcb','wordcc','wordcd','wordce','wordcf','wordcg','wordch','wordci','wordcj','wordck','wordcl','wordcm','wordcn','wordco','wordcp','wordcq','wordcr','wordcs','wordct','wordcu','wordcv','wordcw','wordcx','wordcy','wordcz','wordda','worddb','worddc','worddd','wordde','worddf','worddg','worddh','worddi','worddj','worddk','worddl','worddm','worddn','worddo','worddp','worddq','worddr','wordds','worddt','worddu','worddv','worddw','worddx','worddy','worddz','wordea','wordeb','wordec','worded','wordee','wordef','wordeg','wordeh','wordei','wordej','wordek','wordel','wordem','worden','wordeo','wordep','wordeq','worder','wordes','wordet','wordeu','wordev','wordew','wordex']) WITH ORDINALITY words(w,j) WHERE n % 17 = 0 OR (n*31+j*7)%19 < 5 GROUP BY n;
CREATE INDEX shape_search ON shape_documents USING stannum(body);
CREATE TEMP TABLE shape_allowed(id bigint PRIMARY KEY);
INSERT INTO shape_allowed SELECT n FROM generate_series(1,4096) n WHERE n % 7 = 0;
ANALYZE shape_documents;
ANALYZE shape_allowed;
SELECT d.id,stannum.full_score(d.ctid) AS score FROM shape_documents d WHERE (d.body ==> 'wordaa OR wordab OR wordac OR wordad OR wordae OR wordaf OR wordag OR wordah') AND (EXISTS (SELECT 1 FROM shape_allowed a WHERE a.id=d.id)) ORDER BY score DESC LIMIT 10;
