# The homepage's featured hike for a park, picked as the website picks it
# (hikeratlas/website app/featured.ts, and app/park.ts's difficulty): the
# best-scoring easy or moderate day hike that scores APPROACHABLE_WITHIN of
# the park's best, else the best. Keep the two in step: a change there is a
# change here.

APPROACHABLE_WITHIN = 0.7


def difficulty(route):
    """From the route's hardest SAC grade, its climb and its length: SAC T4
    and up is a scramble; 1,000 m of climb, 18 km or SAC T3 is hard; 400 m
    or 10 km is moderate; less is easy."""
    sac = route.get('max_sac') or 0
    if sac >= 4:
        return 'Scramble'
    if route['gain'] >= 1000 or route['km'] >= 18 or sac == 3:
        return 'Hard'
    if route['gain'] >= 400 or route['km'] >= 10:
        return 'Moderate'
    return 'Easy'


def featured(doc):
    """The park document's featured day hike, or None if it has none."""
    hikes = sorted(doc.get('day_hikes', []), key=lambda h: -(h.get('score') or 0))
    if not hikes:
        return None
    best = hikes[0].get('score') or 0
    return next((h for h in hikes if difficulty(h['route']) in ('Easy', 'Moderate')
                 and (h.get('score') or 0) >= APPROACHABLE_WITHIN * best), hikes[0])


def title(h):
    """What the homepage calls it: its name, else where it goes."""
    return h.get('name') or h.get('dest')


def describe(h):
    r = h['route']
    return f"{title(h)} from {h.get('from')} ({difficulty(r).lower()}, {r['km']} km, {r['gain']:.0f} m, score {h.get('score')})"
