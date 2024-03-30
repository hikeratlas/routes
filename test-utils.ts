import type { NodeMap } from './types';
import * as lib from './lib';

let cachedIndex: NodeMap;
let cachedTrailheads: string[];

export const buildIndexCached = (): NodeMap => {
	if (cachedIndex)
		return cachedIndex;

	const parseTs = Date.now();
	const features = lib.load('../basemap/ztrails.geojsonl');
	console.log(`parse took ${Date.now() - parseTs} ms`);
	const loadTs = Date.now();
	const map: NodeMap = lib.buildIndex(features);
	console.log(`load took ${Date.now() - loadTs} ms`);
	cachedIndex = map;
	return cachedIndex;
}

export const getTrailheadsCached = (): string[] => {
	if (cachedTrailheads)
		return cachedTrailheads;

	const map: NodeMap = buildIndexCached();
	cachedTrailheads = lib.getTrailheads(map);
	return cachedTrailheads;
}
