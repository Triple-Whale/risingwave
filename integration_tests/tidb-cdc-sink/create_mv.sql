--
-- Find the top10 hotest hashtags.
--
CREATE MATERIALIZED VIEW hot_hashtags AS WITH tags AS (
    SELECT
        unnest(regexp_matches(tweet.text, '#\w+', 'g')) AS hashtag,
        tweet.created_at AT TIME ZONE 'UTC' AS created_at
    FROM
        tweet JOIN user
    ON
        tweet.author_id = user.id
)
SELECT
    hashtag,
    COUNT(*) AS hashtag_occurrences,
    window_start
FROM
    TUMBLE(tags, created_at, INTERVAL '5 minute')
GROUP BY
    hashtag,
    window_start
ORDER BY
    hashtag_occurrences;

CREATE MATERIALIZED VIEW datatype_c0_boolean AS
SELECT
    c0_boolean,
    COUNT(*) as c0_count
FROM
    datatype
GROUP BY
    c0_boolean;

CREATE SINK hot_hashtags_sink FROM hot_hashtags
WITH (
   connector='jdbc',
   jdbc.url='jdbc:mysql://tidb:4000/test?user=root&password=',
   table.name='hot_hashtags',
   type='upsert',
   primary_key='window_start,hashtag'
);
